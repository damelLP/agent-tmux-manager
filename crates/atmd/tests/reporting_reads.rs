//! Synthetic reporting-line reads through a real private server, never a harness.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use atm_org::{DecisionCommand, OrgCommand};
use atm_protocol::{ClientMessage, DaemonMessage, ResponseError, ResponsePayload};
use atmd::{
    org::{agents::AgentHandle, OrgHandle},
    registry::spawn_registry,
    server::{methods::MethodServices, DaemonServer},
};
use serde_json::{json, Value};
use std::{fs, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tokio_util::sync::CancellationToken;

const SEATS: [(&str, &str, &str); 8] = [
    ("alpha/worker", "%1", "lead"),
    ("alpha/lead", "%2", "gamma/chief"),
    ("alpha/other", "%3", "lead"),
    ("beta/worker", "%4", "lead"),
    ("beta/lead", "%5", "owner"),
    ("beta/manager", "%6", "gamma/chief"),
    ("gamma/chief", "%7", "owner"),
    ("gamma/other", "%8", "owner"),
];
fn pane(who: &str) -> Option<&str> {
    if who == "owner" {
        return None;
    }
    Some(
        SEATS
            .iter()
            .find(|(id, _, _)| *id == who)
            .expect("known fixture caller")
            .1,
    )
}
fn role(manager: &str) -> String {
    format!("+++\nharness='pi'\nmanager='{manager}'\n+++\nPrivate role text\n")
}
fn request(title: &str) -> Value {
    json!({"kind":"plan","title":title,"context":"Full decision context","files":["decision-proof.md"],"options":["yes","no"],"recommended":"yes","may_be_undone":true,"changes_intent":true})
}
fn decision_ids(page: &Value) -> Vec<String> {
    page["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().into())
        .collect()
}
struct Rig {
    dir: tempfile::TempDir,
    org: OrgHandle,
    org_cancel: CancellationToken,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), atmd::server::ServerError>>,
    control: tokio::task::JoinHandle<()>,
}
impl Rig {
    async fn new(count: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let org_cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), org_cancel.clone()).await;
        for project in ["alpha", "beta", "gamma", "delta", "epsilon"]
            .into_iter()
            .take(count)
        {
            let root = dir.path().join(project);
            fs::create_dir_all(root.join(".atm/roles")).unwrap();
            fs::write(root.join(".atm/project.toml"), format!("id='{project}'\n")).unwrap();
            org.execute(
                OrgCommand::RegisterProject {
                    project_id: project.parse().unwrap(),
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
        assert!(!dir.path().join("gamma/.git").exists());
        let cancel = CancellationToken::new();
        let mut services = MethodServices::new(spawn_registry());
        services.org = org.clone();
        services.agents = Some(AgentHandle::disabled("private reporting fixture"));
        services.delivery_policy = Some(Ok(atm_org::delivery::DeliveryPolicy::default()));
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
            assert!(run.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        Self {
            dir,
            org,
            org_cancel,
            cancel,
            task: tokio::spawn(run),
            control: control_task,
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
    async fn connect(&self) -> BufReader<UnixStream> {
        let stream = UnixStream::connect(self.dir.path().join("atm.sock"))
            .await
            .unwrap();
        let mut read = BufReader::new(stream);
        let connect = ClientMessage::connect(Some("reporting-fixture".into()));
        read.get_mut()
            .write_all(format!("{}\n", serde_json::to_string(&connect).unwrap()).as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["type"],
            "connected"
        );
        read
    }
    async fn rpc(
        read: &mut BufReader<UnixStream>,
        from: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, ResponseError> {
        tokio::time::timeout(Duration::from_secs(60), async {
            let request = ClientMessage::request(
                "read".into(),
                method.into(),
                params,
                from.map(str::to_owned),
            );
            read.get_mut()
                .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
                .await
                .unwrap();
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            match serde_json::from_str::<DaemonMessage>(&line).unwrap() {
                DaemonMessage::Response {
                    payload: ResponsePayload::Result(value),
                    ..
                } => Ok(value),
                DaemonMessage::Response {
                    payload: ResponsePayload::Error(error),
                    ..
                } => Err(error),
                other => panic!("unexpected response: {other:?}"),
            }
        })
        .await
        .unwrap()
    }
    async fn raw(
        &self,
        from: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, ResponseError> {
        tokio::time::timeout(Duration::from_secs(60), async {
            Self::rpc(&mut self.connect().await, from, method, params).await
        })
        .await
        .unwrap()
    }
    async fn call(&self, who: &str, method: &str, params: Value) -> Result<Value, ResponseError> {
        self.raw(pane(who), method, params).await
    }
    async fn ok(&self, who: &str, method: &str, params: Value) -> Value {
        self.call(who, method, params).await.unwrap()
    }
    async fn asked(&self, who: &str) -> String {
        self.ok(who, "decision.request", request(who)).await["decision_id"]
            .as_str()
            .unwrap()
            .into()
    }
    async fn stop(mut self) {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(60), &mut self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        (&mut self.control).await.unwrap();
        self.org_cancel.cancel();
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.org_cancel.cancel();
    }
}

// Break caught: keeping requester-only visibility, or promoting an unrelated lead.
#[tokio::test]
async fn managers_list_and_get_direct_and_transitive_report_decisions() {
    for count in [3, 5] {
        let rig = Rig::new(count).await;
        let worker = rig.asked("alpha/worker").await;
        let lead = rig.asked("alpha/lead").await;
        let chief = rig.asked("gamma/chief").await;
        let beta = rig.asked("beta/lead").await;
        for (who, expected) in [
            ("alpha/lead", vec![worker.clone(), lead.clone()]),
            (
                "gamma/chief",
                vec![worker.clone(), lead.clone(), chief.clone()],
            ),
            ("beta/lead", vec![beta.clone()]),
        ] {
            let listed = rig.ok(who, "decision.list", json!({})).await;
            assert_eq!(decision_ids(&listed), expected, "{who}");
            for id in &expected {
                assert_eq!(
                    rig.ok(who, "decision.get", json!({"id":id})).await,
                    rig.ok("owner", "decision.get", json!({"id":id})).await
                );
            }
        }
        for (who, id) in [
            ("alpha/worker", &lead),
            ("beta/lead", &worker),
            ("alpha/lead", &chief),
        ] {
            assert_eq!(
                rig.call(who, "decision.get", json!({"id":id}))
                    .await
                    .unwrap_err()
                    .code,
                "refused"
            );
        }
        rig.manager("alpha/lead", "beta/manager");
        let got = rig.ok("beta/manager", "decision.list", json!({})).await;
        assert_eq!(decision_ids(&got), [worker, lead]);
        rig.stop().await;
    }
}

// Break caught: counts or cursors include invisible rows, or aggregates lose global scope.
#[tokio::test]
async fn decision_scope_counts_filters_and_cursors_agree() {
    for count in [3, 5] {
        let rig = Rig::new(count).await;
        let first = rig.asked("alpha/worker").await;
        rig.asked("beta/worker").await;
        let own = rig.asked("alpha/lead").await;
        let provisional = rig.asked("alpha/worker").await;
        let withdrawn = rig.asked("alpha/worker").await;
        rig.ok("alpha/worker", "decision.withdraw", json!({"id":withdrawn}))
            .await;
        let mut system = request("System request");
        system["requester"] = json!("system");
        rig.org
            .execute(
                OrgCommand::Decision(DecisionCommand::Request(
                    serde_json::from_value(system).unwrap(),
                )),
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        let mut notice = rig
            .org
            .watch(vec!["msg.sent".into()], rig.org.status().unwrap())
            .unwrap();
        rig.org
            .execute(
                OrgCommand::Decision(DecisionCommand::TakeProvisionally(
                    atm_org::ProvisionalChoice {
                        relayed_by: None,
                        decision_id: provisional.parse().unwrap(),
                        option: "yes".into(),
                        by: "system".parse().unwrap(),
                    },
                )),
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(60), notice.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let mut after = Value::Null;
        for (id, last) in [(&first, false), (&own, false), (&provisional, true)] {
            let page = rig
                .ok(
                    "alpha/lead",
                    "decision.list",
                    json!({"limit":1,"after":after}),
                )
                .await;
            assert_eq!(decision_ids(&page).as_slice(), std::slice::from_ref(id));
            assert_eq!(page["pending_count"], 2);
            assert_eq!(page["provisional_count"], 1);
            assert_eq!(
                page["next_after"],
                if last { Value::Null } else { json!(id) }
            );
            after = json!(id);
        }
        let final_page = rig
            .ok(
                "alpha/lead",
                "decision.list",
                json!({"statuses":["withdrawn"]}),
            )
            .await;
        assert_eq!(
            decision_ids(&final_page).as_slice(),
            std::slice::from_ref(&withdrawn)
        );
        assert_eq!(final_page["pending_count"], 2);
        for who in [
            "owner",
            "alpha/worker",
            "alpha/lead",
            "gamma/chief",
            "beta/lead",
        ] {
            let counts = rig
                .ok(who, "decision.list", json!({"count_only":true,"limit":1}))
                .await;
            assert_eq!(
                counts,
                json!({"decisions":[],"pending_count":4,"provisional_count":1,"next_after":null})
            );
        }
        assert_eq!(
            rig.call("alpha/lead", "decision.get", json!({"id":"d999999"}))
                .await
                .unwrap_err()
                .code,
            "not_found"
        );
        assert_eq!(
            rig.raw(Some("bad-pane"), "decision.get", json!({"id":first}))
                .await
                .unwrap_err()
                .code,
            "invalid_params"
        );
        rig.stop().await;
    }
}

// Break caught: adoption or a saved manager overrides fresh files, or grants are cached.
#[tokio::test]
async fn adopted_and_started_reports_follow_current_roles() {
    for started in [false, true] {
        let rig = Rig::new(3).await;
        if started {
            let id: atm_org::AgentId = "alpha/worker".parse().unwrap();
            for command in [
                OrgCommand::Down {
                    agent_id: id.clone(),
                    reason: "Fixture restart".into(),
                },
                OrgCommand::Up {
                    agent_id: id.clone(),
                    role: atm_org::parse_role("worker", &role("beta/lead")).unwrap(),
                },
                OrgCommand::PaneCreated {
                    agent_id: id,
                    pane: "%1".into(),
                },
            ] {
                rig.org.execute(command, chrono::Utc::now()).await.unwrap();
            }
        } else {
            assert!(rig
                .org
                .snapshot()
                .await
                .unwrap()
                .agent(&"alpha/worker".parse().unwrap())
                .unwrap()
                .role
                .is_none());
        }
        let id = rig.asked("alpha/worker").await;
        rig.ok("alpha/lead", "decision.get", json!({"id":id})).await;
        assert_eq!(
            rig.call("beta/lead", "decision.get", json!({"id":id}))
                .await
                .unwrap_err()
                .code,
            "refused"
        );
        rig.manager("alpha/worker", "beta/lead");
        rig.ok("beta/lead", "decision.get", json!({"id":id})).await;
        assert_eq!(
            rig.call("alpha/lead", "decision.get", json!({"id":id}))
                .await
                .unwrap_err()
                .code,
            "refused"
        );
        fs::remove_file(rig.path("alpha/worker")).unwrap();
        assert_eq!(
            rig.call("beta/lead", "decision.get", json!({"id":id}))
                .await
                .unwrap_err()
                .code,
            "refused"
        );
        rig.ok("alpha/worker", "decision.get", json!({"id":id}))
            .await;
        rig.ok("owner", "decision.get", json!({"id":id})).await;
        rig.manager("alpha/worker", "lead");
        fs::remove_file(rig.path("alpha/lead")).unwrap();
        rig.ok("alpha/lead", "decision.get", json!({"id":id})).await;
        assert_eq!(
            rig.call("gamma/chief", "decision.get", json!({"id":id}))
                .await
                .unwrap_err()
                .code,
            "refused"
        );
        rig.stop().await;
    }
}

// Break caught: a broken tail grants a partial ancestor or erases unrelated valid/own rows.
#[tokio::test]
async fn broken_reporting_lines_never_grant_decision_reads() {
    for (node, manager) in [
        ("alpha/worker", "alpha/worker"),
        ("alpha/lead", "alpha/worker"),
        ("gamma/chief", "alpha/lead"),
        ("gamma/chief", "beta/unknown"),
        ("gamma/chief", "../invalid"),
    ] {
        let rig = Rig::new(3).await;
        let child = rig.asked("alpha/worker").await;
        let own = rig.asked("alpha/lead").await;
        rig.manager(node, manager);
        let error = rig
            .call("alpha/lead", "decision.get", json!({"id":child}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "refused");
        assert!(error.message.contains(node), "{error:?}");
        assert!(!error.message.contains("Private role text"));
        assert_eq!(
            decision_ids(&rig.ok("alpha/lead", "decision.list", json!({})).await),
            [own]
        );
        rig.ok("owner", "decision.get", json!({"id":child})).await;
        rig.ok("alpha/worker", "decision.get", json!({"id":child}))
            .await;
        rig.stop().await;
    }
    for case in ["invalid", "utf8", "directory", "oversized", "missing-root"] {
        let rig = Rig::new(5).await;
        let child = rig.asked("alpha/worker").await;
        let other = rig.asked("alpha/other").await;
        let own = rig.asked("alpha/lead").await;
        let path = rig.path("alpha/worker");
        match case {
            "invalid" => fs::write(&path, "Secret malformed config").unwrap(),
            "utf8" => fs::write(&path, [0xff]).unwrap(),
            "directory" => {
                fs::remove_file(&path).unwrap();
                fs::create_dir(&path).unwrap();
            }
            "oversized" => fs::write(&path, vec![b'x'; 256 * 1024 + 1]).unwrap(),
            "missing-root" => {
                // Break beta's unrelated root, then point this report into that chain.
                rig.manager("alpha/worker", "beta/manager");
                fs::rename(
                    rig.dir.path().join("beta"),
                    rig.dir.path().join("moved-beta"),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let error = rig
            .call("alpha/lead", "decision.get", json!({"id":child}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "refused");
        assert!(!error.message.contains("Secret"));
        assert_eq!(
            decision_ids(&rig.ok("alpha/lead", "decision.list", json!({})).await),
            [other, own]
        );
        let counts = rig
            .ok("alpha/worker", "decision.list", json!({"count_only":true}))
            .await;
        assert_eq!(counts["pending_count"], 3);
        rig.stop().await;
    }
}

// Break caught: a permission failure erases own/other rows or becomes infrastructure failure.
#[tokio::test]
async fn unreadable_requester_only_omits_its_own_reporting_grant() {
    use std::os::unix::fs::PermissionsExt;
    for count in [3, 5] {
        let rig = Rig::new(count).await;
        let child = rig.asked("alpha/worker").await;
        let other = rig.asked("alpha/other").await;
        let own = rig.asked("alpha/lead").await;
        let path = rig.path("alpha/worker");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&path).is_ok() {
            eprintln!("skipping unreadable requester case: chmod 000 does not prevent this user from opening the file");
            rig.stop().await;
            continue;
        }
        let page = rig.ok("alpha/lead", "decision.list", json!({})).await;
        assert_eq!(decision_ids(&page), [other, own]);
        assert_eq!(page["pending_count"], 2);
        assert_eq!(
            rig.call("alpha/lead", "decision.get", json!({"id":child}))
                .await
                .unwrap_err()
                .code,
            "refused"
        );
        assert_eq!(
            rig.call("alpha/lead", "msg.inbox", json!({"for":"alpha/worker"}))
                .await
                .unwrap_err()
                .code,
            "refused"
        );
        rig.ok("alpha/worker", "decision.get", json!({"id":child}))
            .await;
        assert_eq!(
            rig.ok("owner", "decision.list", json!({})).await["pending_count"],
            3
        );
        assert_eq!(
            rig.ok("alpha/lead", "decision.list", json!({"count_only":true}))
                .await["pending_count"],
            3
        );
        rig.stop().await;
    }
}

// Break caught: EMFILE is omitted as broken configuration, silently shrinking a complete union.
// A child owns the process-wide limit; handshakes finish before lowering it, not after a delay.
#[cfg(target_os = "linux")]
#[test]
fn reporting_io_failures_never_return_partial_decision_union() {
    const CHILD: &str = "ATM_TEST_REPORTING_FD_CHILD";
    const TEST: &str = "reporting_io_failures_never_return_partial_decision_union";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new("timeout")
            .arg("180")
            .arg(std::env::current_exe().unwrap())
            .args([TEST, "--exact", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        eprint!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "isolated FD probe failed: {}",
            output.status
        );
        return;
    }
    struct FdLimit(libc::rlimit);
    impl FdLimit {
        fn exhaust() -> Self {
            let mut previous = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: valid writable rlimit; this isolated child is its only owner.
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut previous) },
                0
            );
            let limited = libc::rlimit {
                rlim_cur: 0,
                ..previous
            };
            // SAFETY: only lower the soft limit; retain the hard limit and restore on drop.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limited) }, 0);
            Self(previous)
        }
    }
    impl Drop for FdLimit {
        fn drop(&mut self) {
            // SAFETY: restore the saved soft/hard limits even while unwinding this child.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) }, 0);
        }
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for count in [3, 5] {
            let rig = Rig::new(count).await;
            let child = rig.asked("alpha/worker").await;
            let own = rig.asked("alpha/lead").await;
            let events = rig.org.execute(OrgCommand::Task(atm_org::TaskCommand::Create(atm_org::TaskCreated {
                project_id: None, title: "Work".into(), kind: atm_org::TaskKind::Work, creator: "owner".parse().unwrap(),
                parent: None, judges: None, measure: atm_org::TaskMeasure::Mechanical { command: "check".into() },
                budget: None, files: vec![], rests_on: None,
            })), chrono::Utc::now()).await.unwrap();
            let task: atm_org::TaskId = format!("t{}", events.last().unwrap().seq).parse().unwrap();
            rig.org.execute(OrgCommand::Task(atm_org::TaskCommand::Assign {
                task_id: task.clone(), agent_id: "alpha/worker".parse().unwrap(),
                by: atm_org::TaskActor { who: "owner".parse().unwrap(), manager_of: None },
            }), chrono::Utc::now()).await.unwrap();
            let baseline = rig.ok("alpha/lead", "decision.list", json!({})).await;
            assert_eq!(decision_ids(&baseline), [child.clone(), own.clone()]);
            assert_eq!(baseline["pending_count"], 2);
            eprintln!("FD probe baseline ({count} projects): {baseline}");
            let mut connection = tokio::time::timeout(Duration::from_secs(60), rig.connect()).await.unwrap();
            let limit = FdLimit::exhaust();
            let error = fs::File::open(rig.path("alpha/worker")).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
            let list = Rig::rpc(&mut connection, pane("alpha/lead"), "decision.list", json!({})).await;
            let get = Rig::rpc(&mut connection, pane("alpha/lead"), "decision.get", json!({"id":child})).await;
            let inbox = Rig::rpc(&mut connection, pane("alpha/lead"), "msg.inbox", json!({"for":"alpha/worker"})).await;
            let authority = Rig::rpc(&mut connection, pane("alpha/lead"), "task.set_state", json!({"task_id":task,"state":"cancelled"})).await;
            drop(limit);
            eprintln!("FD probe exhausted: list={list:?}; get={get:?}; inbox={inbox:?}; task={authority:?}");
            let recovered = rig.ok("alpha/lead", "decision.list", json!({})).await;
            assert_eq!(decision_ids(&recovered), [child, own]);
            assert_eq!(recovered["pending_count"], 2);
            eprintln!("FD probe recovered: {recovered}");
            for result in [list, get, inbox] {
                assert_eq!(result.unwrap_err().code, "internal");
            }
            let authority = authority.unwrap_err();
            assert_eq!(authority.code, "refused");
            assert_eq!(authority.message, format!(
                "manager authority for alpha/worker, reading alpha/worker: reporting line at alpha/worker: agent operation failed: {}: {}",
                rig.path("alpha/worker").display(), std::io::Error::from_raw_os_error(libc::EMFILE)
            ));
            drop(connection);
            rig.stop().await;
        }
    });
}

// Break caught: merging redo paging drops ancestor reads or grants owner-only redo data.
#[tokio::test]
async fn manager_decision_reads_preserve_owner_only_redo_paging() {
    let rig = Rig::new(3).await;
    let id = rig.asked("alpha/worker").await;
    rig.org
        .execute(
            OrgCommand::Decision(DecisionCommand::TakeProvisionally(
                atm_org::ProvisionalChoice {
                    relayed_by: None,
                    decision_id: id.parse().unwrap(),
                    option: "yes".into(),
                    by: "system".parse().unwrap(),
                },
            )),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    rig.ok("owner", "decision.resolve", json!({"id":id,"option":"no"}))
        .await;
    let owner = rig.ok("owner", "decision.get", json!({"id":id})).await;
    assert_eq!(owner["decision"]["status"], "overruled");
    assert!(owner["redo"].is_object());
    for who in ["alpha/worker", "alpha/lead", "gamma/chief"] {
        let got = rig.ok(who, "decision.get", json!({"id":id})).await;
        assert_eq!(got["decision"], owner["decision"], "{who}");
        assert!(got.get("redo").is_none(), "{who}");
    }
    let page = rig
        .ok("owner", "decision.get", json!({"id":id,"redo_limit":1}))
        .await;
    assert!(page["redo"].is_object());
    // Paging must refuse before consulting even a broken reporting chain.
    rig.manager("alpha/worker", "beta/missing");
    for who in ["alpha/worker", "alpha/lead", "gamma/chief", "beta/lead"] {
        let error = rig
            .call(who, "decision.get", json!({"id":id,"redo_limit":1}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "refused", "{who}");
        assert_eq!(error.message, "only the owner may page redo tasks", "{who}");
        let digest = rig
            .call(who, "owner.digest", json!({"snapshot_seq":0}))
            .await
            .unwrap_err();
        assert_eq!(digest.code, "refused", "{who}");
        assert_eq!(
            digest.message, "only the owner may read a return digest",
            "{who}"
        );
    }
    rig.stop().await;
}

// Break caught: a read grant is reused as owner/requester mutation authority.
#[tokio::test]
async fn reporting_read_grants_never_grant_mutations() {
    let rig = Rig::new(3).await;
    let pending = rig.asked("alpha/worker").await;
    let provisional = rig.asked("alpha/worker").await;
    let mut notice = rig
        .org
        .watch(vec!["msg.sent".into()], rig.org.status().unwrap())
        .unwrap();
    rig.org
        .execute(
            OrgCommand::Decision(DecisionCommand::TakeProvisionally(
                atm_org::ProvisionalChoice {
                    relayed_by: None,
                    decision_id: provisional.parse().unwrap(),
                    option: "yes".into(),
                    by: "system".parse().unwrap(),
                },
            )),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), notice.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    for id in [&pending, &provisional] {
        rig.ok("gamma/chief", "decision.get", json!({"id":id}))
            .await;
        let before = rig.org.snapshot().await.unwrap();
        let cursor = rig.org.status().unwrap();
        for (method, params) in [
            ("decision.resolve", json!({"id":id,"option":"yes"})),
            ("decision.withdraw", json!({"id":id})),
        ] {
            assert_eq!(
                rig.call("gamma/chief", method, params)
                    .await
                    .unwrap_err()
                    .code,
                "refused"
            );
        }
        assert_eq!(rig.org.snapshot().await.unwrap(), before);
        assert_eq!(rig.org.status().unwrap(), cursor);
    }
    rig.ok("alpha/worker", "decision.withdraw", json!({"id":pending}))
        .await;
    let mut answer = rig
        .org
        .watch(vec!["msg.sent".into()], rig.org.status().unwrap())
        .unwrap();
    rig.ok(
        "owner",
        "decision.resolve",
        json!({"id":provisional,"option":"no","note":"Full owner answer"}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(60), answer.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let got = rig
        .ok("gamma/chief", "decision.get", json!({"id":provisional}))
        .await;
    assert_eq!(got["decision"]["resolution"]["note"], "Full owner answer");
    assert_eq!(
        got["decision"],
        rig.ok("owner", "decision.get", json!({"id":provisional}))
            .await["decision"]
    );
    assert!(got.get("redo").is_none());
    let sent = rig
        .ok(
            "owner",
            "msg.send",
            json!({"to":"alpha/worker","body":"SECRET delivered body"}),
        )
        .await;
    let message_id: atm_org::MessageId =
        serde_json::from_value(sent["message_id"].clone()).unwrap();
    for command in [
        OrgCommand::DeliveryAttempted {
            message_id,
            attempt: 1,
            pane: "%1".into(),
        },
        OrgCommand::Delivered {
            message_id,
            session_id: "fixture-session".into(),
            confirmation: "SECRET confirmation".into(),
        },
    ] {
        rig.org.execute(command, chrono::Utc::now()).await.unwrap();
    }
    // A returned send fences the message fold past the direct delivery writes.
    rig.ok(
        "owner",
        "msg.send",
        json!({"to":"owner","body":"Fold fence"}),
    )
    .await;
    let page = rig
        .ok(
            "gamma/chief",
            "msg.inbox",
            json!({"for":"alpha/worker","id":message_id}),
        )
        .await;
    assert_redacted(&page["messages"][0]);
    assert_eq!(page["messages"][0]["status"], "delivered");
    let before = rig.org.snapshot().await.unwrap();
    let cursor = rig.org.status().unwrap();
    for (method, params) in [
        ("msg.ack", json!({"ids":[message_id]})),
        ("msg.retry", json!({"id":message_id})),
        ("msg.give_up", json!({"id":message_id})),
    ] {
        assert_eq!(
            rig.call("gamma/chief", method, params)
                .await
                .unwrap_err()
                .code,
            "refused"
        );
    }
    assert_eq!(rig.org.snapshot().await.unwrap(), before);
    assert_eq!(rig.org.status().unwrap(), cursor);
    assert_eq!(
        rig.ok("alpha/worker", "msg.ack", json!({"ids":[message_id]}))
            .await["count"],
        1
    );
    assert!(rig
        .org
        .snapshot()
        .await
        .unwrap()
        .message(&message_id)
        .unwrap()
        .read_at
        .is_some());
    rig.stop().await;
}

// Break caught: inbox admission diverges from decisions on fresh/adopted/broken roles.
#[tokio::test]
async fn inbox_roles_follow_the_same_current_chain_as_decisions() {
    for started in [false, true] {
        let rig = Rig::new(3).await;
        if started {
            let id: atm_org::AgentId = "alpha/worker".parse().unwrap();
            for command in [
                OrgCommand::Down {
                    agent_id: id.clone(),
                    reason: "Fixture restart".into(),
                },
                OrgCommand::Up {
                    agent_id: id.clone(),
                    role: atm_org::parse_role("worker", &role("beta/lead")).unwrap(),
                },
                OrgCommand::PaneCreated {
                    agent_id: id,
                    pane: "%1".into(),
                },
            ] {
                rig.org.execute(command, chrono::Utc::now()).await.unwrap();
            }
        }
        let id = rig.asked("alpha/worker").await;
        rig.ok(
            "owner",
            "msg.send",
            json!({"to":"alpha/worker","body":"SECRET body"}),
        )
        .await;
        for (manager, allowed, denied) in [
            ("lead", "alpha/lead", "beta/lead"),
            ("beta/lead", "beta/lead", "alpha/lead"),
        ] {
            rig.manager("alpha/worker", manager);
            rig.ok(allowed, "decision.get", json!({"id":id})).await;
            assert_redacted(
                &rig.ok(allowed, "msg.inbox", json!({"for":"alpha/worker"}))
                    .await["messages"][0],
            );
            for method in ["decision.get", "msg.inbox"] {
                let params = if method == "decision.get" {
                    json!({"id":id})
                } else {
                    json!({"for":"alpha/worker"})
                };
                assert_eq!(
                    rig.call(denied, method, params).await.unwrap_err().code,
                    "refused"
                );
            }
        }
        fs::remove_file(rig.path("alpha/worker")).unwrap();
        assert_eq!(
            rig.call("beta/lead", "msg.inbox", json!({"for":"alpha/worker"}))
                .await
                .unwrap_err()
                .code,
            "refused"
        );
        rig.manager("alpha/worker", "lead");
        fs::remove_file(rig.path("alpha/lead")).unwrap();
        rig.ok("alpha/lead", "msg.inbox", json!({"for":"alpha/worker"}))
            .await;
        assert_eq!(
            rig.call("gamma/chief", "msg.inbox", json!({"for":"alpha/worker"}))
                .await
                .unwrap_err()
                .code,
            "refused"
        );
        rig.stop().await;
    }
    for case in [
        "cycle",
        "unknown",
        "invalid",
        "utf8",
        "directory",
        "symlink",
        "oversized",
        "unreadable",
        "missing-root",
    ] {
        let rig = Rig::new(5).await;
        let id = rig.asked("alpha/worker").await;
        let path = rig.path("gamma/chief");
        match case {
            "cycle" => rig.manager("gamma/chief", "alpha/lead"),
            "unknown" => rig.manager("gamma/chief", "beta/unknown"),
            "invalid" => fs::write(&path, "SECRET malformed config").unwrap(),
            "utf8" => fs::write(&path, [0xff]).unwrap(),
            "directory" => {
                fs::remove_file(&path).unwrap();
                fs::create_dir(&path).unwrap();
            }
            "symlink" => {
                fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(rig.path("alpha/worker"), &path).unwrap();
            }
            "oversized" => fs::write(&path, vec![b'x'; 256 * 1024 + 1]).unwrap(),
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
            "missing-root" => fs::rename(
                rig.dir.path().join("gamma"),
                rig.dir.path().join("moved-gamma"),
            )
            .unwrap(),
            _ => unreachable!(),
        }
        for method in ["decision.get", "msg.inbox"] {
            let params = if method == "decision.get" {
                json!({"id":id})
            } else {
                json!({"for":"alpha/worker"})
            };
            let error = rig.call("alpha/lead", method, params).await.unwrap_err();
            assert_eq!(error.code, "refused", "{case}: {error:?}");
            assert!(error.message.contains("gamma/chief"), "{case}: {error:?}");
            assert!(!error.message.contains("SECRET"));
        }
        rig.ok("owner", "msg.inbox", json!({"for":"alpha/worker"}))
            .await;
        rig.ok("alpha/worker", "msg.inbox", json!({})).await;
        rig.stop().await;
    }
}

// Break: history broadens manager scope to outgoing bodies, or stays incoming-only for self.
#[tokio::test]
async fn history_socket_scopes_and_read_only_contract() {
    for count in [3, 5] {
        let rig = Rig::new(count).await;
        let incoming = history_send(&rig.org, "beta/lead", "alpha/worker", "SECRET incoming").await;
        let outgoing = history_send(&rig.org, "alpha/worker", "owner", "SECRET outgoing").await;
        let self_send = history_send(&rig.org, "alpha/worker", "alpha/worker", "SECRET self").await;
        let other = history_send(&rig.org, "beta/lead", "owner", "SECRET unrelated").await;
        let system = history_send(&rig.org, "system", "alpha/worker", "SECRET system").await;
        let before = rig.org.snapshot().await.unwrap();
        let seq = rig.org.status().unwrap();
        let all = rig.ok("owner", "msg.history", json!({})).await;
        assert_eq!(
            history_ids(&all),
            [
                system.clone(),
                other,
                self_send.clone(),
                outgoing.clone(),
                incoming.clone()
            ]
        );
        assert_eq!(all["seq"], seq);
        assert_eq!(all["snapshot_seq"], seq);
        for who in ["alpha/worker", "owner"] {
            let page = rig
                .ok(who, "msg.history", json!({"agent":"alpha/worker"}))
                .await;
            assert_eq!(
                history_ids(&page),
                [
                    system.clone(),
                    self_send.clone(),
                    outgoing.clone(),
                    incoming.clone()
                ]
            );
            assert_eq!(page["visibility"], "full");
            assert!(page.to_string().contains("SECRET"));
            if who == "alpha/worker" {
                assert_eq!(page, rig.ok(who, "msg.history", json!({})).await);
            }
        }
        for who in ["alpha/lead", "gamma/chief"] {
            let page = rig
                .ok(who, "msg.history", json!({"agent":"alpha/worker"}))
                .await;
            assert_eq!(
                history_ids(&page),
                [system.clone(), self_send.clone(), incoming.clone()]
            );
            assert_eq!(page["visibility"], "delivery");
            for row in page["messages"].as_array().unwrap() {
                assert_redacted(row);
            }
            let sender = rig
                .ok(
                    who,
                    "msg.history",
                    json!({"agent":"alpha/worker", "correspondent":"beta/lead"}),
                )
                .await;
            assert_eq!(history_ids(&sender), std::slice::from_ref(&incoming));
        }
        for (who, agent, code) in [
            ("beta/lead", "alpha/worker", "refused"),
            ("alpha/worker", "alpha/lead", "refused"),
            ("alpha/lead", "beta/worker", "refused"),
            ("alpha/lead", "alpha/missing", "not_found"),
            ("gamma/chief", "owner", "invalid_params"),
        ] {
            assert_eq!(
                rig.call(who, "msg.history", json!({"agent":agent}))
                    .await
                    .unwrap_err()
                    .code,
                code
            );
        }
        assert_eq!(
            rig.raw(Some("not-pane"), "msg.history", json!({}))
                .await
                .unwrap_err()
                .code,
            "invalid_params"
        );
        assert_eq!(rig.org.snapshot().await.unwrap(), before);
        assert_eq!(rig.org.status().unwrap(), seq);
        rig.stop().await;
    }
}

async fn history_send(org: &OrgHandle, from: &str, to: &str, body: &str) -> String {
    let events = org
        .execute(
            OrgCommand::SendMessage(atm_org::MessageContent {
                delivery_notice: None,
                from: from.parse().unwrap(),
                to: to.parse().unwrap(),
                kind: atm_org::MessageKind::Report,
                body: body.into(),
                attachments: vec!["SECRET attachment".into()],
                task: Some("SECRET task".parse().unwrap()),
            }),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    format!("m{}", events[0].seq)
}
fn history_ids(page: &Value) -> Vec<String> {
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["message_id"].as_str().unwrap().into())
        .collect()
}

// Break: cursor replaces admission, saved roles override disk, or a partial chain grants access.
#[tokio::test]
async fn history_reauthorizes_each_page_from_current_roles() {
    for started in [false, true] {
        let rig = Rig::new(3).await;
        if started {
            for command in [
                OrgCommand::Down {
                    agent_id: "alpha/worker".parse().unwrap(),
                    reason: "fixture".into(),
                },
                OrgCommand::Up {
                    agent_id: "alpha/worker".parse().unwrap(),
                    role: atm_org::parse_role("worker", &role("beta/lead")).unwrap(),
                },
                OrgCommand::PaneCreated {
                    agent_id: "alpha/worker".parse().unwrap(),
                    pane: "%1".into(),
                },
            ] {
                rig.org.execute(command, chrono::Utc::now()).await.unwrap();
            }
        }
        history_send(&rig.org, "owner", "alpha/worker", "SECRET one").await;
        history_send(&rig.org, "owner", "alpha/worker", "SECRET two").await;
        let first = rig
            .ok(
                "gamma/chief",
                "msg.history",
                json!({"agent":"alpha/worker","limit":1}),
            )
            .await;
        let next = json!({"agent":"alpha/worker","limit":1,"cursor":first["next_cursor"]});
        assert!(next["cursor"].is_string());
        assert_eq!(
            rig.call("owner", "msg.history", next.clone())
                .await
                .unwrap_err()
                .code,
            "invalid_params"
        );
        rig.manager("alpha/worker", "beta/lead");
        assert_eq!(
            rig.call("gamma/chief", "msg.history", next.clone())
                .await
                .unwrap_err()
                .code,
            "refused"
        );
        rig.ok("beta/lead", "msg.history", json!({"agent":"alpha/worker"}))
            .await;
        rig.manager("alpha/worker", "lead");
        for manager in ["alpha/lead", "beta/unknown", "../invalid"] {
            rig.manager("gamma/chief", manager);
            let error = rig
                .call("gamma/chief", "msg.history", next.clone())
                .await
                .unwrap_err();
            assert_eq!(error.code, "refused");
            assert!(!error.message.contains("SECRET"));
        }
        rig.manager("gamma/chief", "owner");
        rig.ok("gamma/chief", "msg.history", next.clone()).await;
        fs::remove_file(rig.path("alpha/lead")).unwrap();
        assert_eq!(
            rig.call("gamma/chief", "msg.history", next.clone())
                .await
                .unwrap_err()
                .code,
            "refused"
        );
        rig.ok("alpha/lead", "msg.history", json!({"agent":"alpha/worker"}))
            .await;
        fs::remove_file(rig.path("alpha/worker")).unwrap();
        assert_eq!(
            rig.call("alpha/lead", "msg.history", json!({"agent":"alpha/worker"}))
                .await
                .unwrap_err()
                .code,
            "refused"
        );
        rig.ok("owner", "msg.history", json!({"agent":"alpha/worker"}))
            .await;
        rig.ok("alpha/worker", "msg.history", json!({})).await;
        rig.stop().await;
    }
}

// Break: file failures leak role content or silently fall back to saved authority.
#[tokio::test]
async fn history_broken_files_fail_closed() {
    for case in ["utf8", "directory", "symlink", "oversized", "unreadable"] {
        let rig = Rig::new(5).await;
        let path = rig.path("gamma/chief");
        match case {
            "utf8" => fs::write(&path, [0xff]).unwrap(),
            "directory" => {
                fs::remove_file(&path).unwrap();
                fs::create_dir(&path).unwrap();
            }
            "symlink" => {
                fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(rig.path("alpha/worker"), &path).unwrap();
            }
            "oversized" => fs::write(&path, vec![b'x'; 256 * 1024 + 1]).unwrap(),
            "unreadable" => {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
                if fs::File::open(&path).is_ok() {
                    eprintln!("skipping unreadable case: current user can read chmod 000");
                    rig.stop().await;
                    continue;
                }
            }
            _ => unreachable!(),
        }
        let error = rig
            .call("alpha/lead", "msg.history", json!({"agent":"alpha/worker"}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "refused", "{case}: {error:?}");
        assert!(!error.message.contains("SECRET"));
        rig.ok("owner", "msg.history", json!({})).await;
        rig.ok("alpha/worker", "msg.history", json!({})).await;
        rig.stop().await;
    }
}

// Break: an actual reporting I/O failure becomes a scope refusal or an empty successful page.
#[cfg(target_os = "linux")]
#[test]
fn history_reporting_io_failure_is_internal() {
    const CHILD: &str = "ATM_TEST_HISTORY_FD";
    const TEST: &str = "history_reporting_io_failure_is_internal";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new("timeout")
            .arg("60")
            .arg(std::env::current_exe().unwrap())
            .args([TEST, "--exact", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let rig = Rig::new(3).await;
            let mut read = rig.connect().await;
            let mut previous = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: valid rlimit pointers; this isolated child owns the process-wide soft limit.
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut previous) },
                0
            );
            let limited = libc::rlimit {
                rlim_cur: 0,
                ..previous
            };
            // SAFETY: retain the hard limit and restore before any result assertion.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limited) }, 0);
            let result = Rig::rpc(
                &mut read,
                pane("alpha/lead"),
                "msg.history",
                json!({"agent":"alpha/worker"}),
            )
            .await;
            // SAFETY: restore the previously saved limits in the isolated child.
            assert_eq!(
                unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &previous) },
                0
            );
            assert_eq!(result.unwrap_err().code, "internal");
            rig.ok("alpha/lead", "msg.history", json!({"agent":"alpha/worker"}))
                .await;
            rig.stop().await;
        });
}

// Break: complete response line exceeds the wire bound even though the result fits.
#[tokio::test]
async fn history_wire_bound_accounts_for_request_id() {
    let rig = Rig::new(3).await;
    history_send(&rig.org, "owner", "owner", &"x".repeat(400_000)).await;
    let mut read = rig.connect().await;
    let id = "i".repeat(700_000);
    let request = ClientMessage::request(id.clone(), "msg.history".into(), json!({}), None);
    read.get_mut()
        .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(60), read.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert!(line.len() <= 1_048_576);
    let reply: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["id"], id);
    assert_eq!(reply["error"]["code"], "refused");
    // The largest valid request may not leave even the refusal's envelope room.
    let mut connection = rig.connect().await;
    let empty = ClientMessage::request(String::new(), "msg.history".into(), json!({}), None);
    let overhead = serde_json::to_string(&empty).unwrap().len() + 1;
    let huge = ClientMessage::request(
        "i".repeat(1_048_576 - overhead),
        "msg.history".into(),
        json!({}),
        None,
    );
    connection
        .get_mut()
        .write_all(format!("{}\n", serde_json::to_string(&huge).unwrap()).as_bytes())
        .await
        .unwrap();
    let mut refused = String::new();
    tokio::time::timeout(Duration::from_secs(60), connection.read_line(&mut refused))
        .await
        .unwrap()
        .unwrap();
    assert!(refused.len() <= 1_048_576);
    if !refused.is_empty() {
        let error: Value = serde_json::from_str(&refused).unwrap();
        assert_eq!(error["type"], "error");
    }
    let info = rig.ok("owner", "daemon.info", json!({})).await;
    assert!(info["methods"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m == "msg.history"));
    rig.stop().await;
}

fn assert_redacted(row: &Value) {
    assert_eq!(row["content"]["body"], "");
    assert_eq!(row["content"]["attachments"], json!([]));
    assert_eq!(row["content"]["task"], Value::Null);
    assert_eq!(row["delivery"]["reason"], Value::Null);
    assert_eq!(row["delivery"]["confirmation"], Value::Null);
    assert!(
        !row.to_string().contains("SECRET"),
        "private data leaked: {row}"
    );
}

// Break caught: manager access stays refused, leaks content, or affects owner/self views.
#[tokio::test]
async fn manager_inbox_is_metadata_only_on_every_read_path() {
    for count in [3, 5] {
        let rig = Rig::new(count).await;
        let content = json!({"to":"alpha/worker","kind":"report","body":"SECRET body","attachments":["SECRET-attachment.md"],"task":"SECRET-task-reference"});
        let first = rig.ok("owner", "msg.send", content.clone()).await["message_id"].clone();
        let second = rig.ok("owner", "msg.send", content.clone()).await["message_id"].clone();
        rig.ok(
            "owner",
            "msg.send",
            json!({"to":"alpha/lead","body":"Own SECRET content"}),
        )
        .await;
        rig.ok(
            "alpha/worker",
            "msg.send",
            json!({"to":"owner","body":"Owner SECRET content"}),
        )
        .await;
        let original = rig
            .ok("owner", "msg.inbox", json!({"for":"alpha/worker"}))
            .await;
        assert_eq!(
            original,
            rig.ok("alpha/worker", "msg.inbox", json!({})).await
        );
        assert_eq!(original["messages"][0]["content"]["body"], "SECRET body");
        assert_eq!(
            original["messages"][0]["content"]["attachments"],
            content["attachments"]
        );
        assert_eq!(original["messages"][0]["content"]["task"], content["task"]);
        for who in ["alpha/lead", "gamma/chief"] {
            for params in [
                json!({"for":"alpha/worker"}),
                json!({"for":"alpha/worker","id":first}),
                json!({"for":"alpha/worker","unread_only":true}),
                json!({"for":"alpha/worker","limit":1}),
                json!({"for":"alpha/worker","after":first,"limit":1}),
            ] {
                let page = rig.ok(who, "msg.inbox", params.clone()).await;
                assert_eq!(page["unread_count"], 2);
                let rows = page["messages"].as_array().unwrap();
                assert!(!rows.is_empty());
                for row in rows {
                    assert_redacted(row);
                    assert_eq!(row["content"]["from"], "owner");
                    assert_eq!(row["content"]["to"], "alpha/worker");
                    assert_eq!(row["content"]["kind"], "report");
                }
                if params["after"] == first {
                    assert_eq!(rows[0]["message_id"], second);
                }
                if params["limit"] == 1 && params["after"].is_null() {
                    assert_eq!(page["next_after"], first);
                }
            }
            assert_eq!(
                rig.ok(
                    who,
                    "msg.inbox",
                    json!({"for":"alpha/worker","count_only":true})
                )
                .await,
                json!({"messages":[],"unread_count":2,"next_after":null})
            );
            assert_eq!(
                rig.ok(who, "msg.inbox", json!({"for":"owner","count_only":true}))
                    .await["unread_count"],
                1
            );
            assert_eq!(
                rig.call(who, "msg.inbox", json!({"for":"owner"}))
                    .await
                    .unwrap_err()
                    .code,
                "refused"
            );
        }
        let own = rig.ok("alpha/lead", "msg.inbox", json!({})).await;
        assert_eq!(own["messages"][0]["content"]["body"], "Own SECRET content");
        for (who, recipient) in [
            ("beta/lead", "alpha/worker"),
            ("alpha/worker", "alpha/lead"),
        ] {
            for count_only in [false, true] {
                assert_eq!(
                    rig.call(
                        who,
                        "msg.inbox",
                        json!({"for":recipient,"count_only":count_only})
                    )
                    .await
                    .unwrap_err()
                    .code,
                    "refused"
                );
            }
        }
        assert_eq!(
            rig.call("alpha/lead", "msg.inbox", json!({"for":"alpha/unknown"}))
                .await
                .unwrap_err()
                .code,
            "not_found"
        );
        rig.stop().await;
    }
}
