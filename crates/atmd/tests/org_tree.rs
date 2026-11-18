//! Synthetic neutral projects over the real router/connection, with private files.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use atm_org::{Address, OrgCommand};
use atm_protocol::ClientMessage;
use atmd::{
    org::OrgHandle,
    registry::{spawn_registry, RegistryHandle},
    server::{
        methods::{router, CallContext, CallerId, CallerPane, MethodRouter, MethodServices},
        ConnectionHandler,
    },
};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::RwLock,
};
use tokio_util::sync::CancellationToken;

struct Rig {
    dir: tempfile::TempDir,
    org: OrgHandle,
    registry: RegistryHandle,
    methods: Arc<MethodRouter>,
    cancel: CancellationToken,
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl Rig {
    async fn new(count: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        for id in ["alpha", "beta", "gamma", "delta", "epsilon"]
            .into_iter()
            .take(count)
        {
            let root = dir.path().join(id);
            std::fs::create_dir_all(root.join(".atm/roles")).unwrap();
            org.execute(
                OrgCommand::RegisterProject {
                    project_id: id.parse().unwrap(),
                    root: root.to_str().unwrap().into(),
                    move_root: false,
                },
                now(),
            )
            .await
            .unwrap();
        }
        assert!(!dir.path().join("gamma/.git").exists());
        for (id, pane, manager) in [
            ("alpha/lead", "%1", "gamma/chief"),
            ("alpha/worker", "%2", "lead"),
            ("beta/broken", "%3", "owner"),
            ("gamma/chief", "%4", "owner"),
        ] {
            org.execute(
                OrgCommand::Adopt {
                    agent_id: id.parse().unwrap(),
                    pane: pane.into(),
                    session_id: Some(format!("session-{pane}").into()),
                },
                now(),
            )
            .await
            .unwrap();
            let (project, name) = id.split_once('/').unwrap();
            std::fs::write(
                dir.path()
                    .join(project)
                    .join(format!(".atm/roles/{name}.md")),
                format!("+++\nharness='pi'\nmanager='{manager}'\n+++\nSECRET role instructions\n"),
            )
            .unwrap();
        }
        std::fs::write(
            dir.path().join("beta/.atm/roles/broken.md"),
            "+++\nharness=17\n+++\nSECRET invalid role\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("alpha/.atm/roles/unregistered.md"),
            "+++\nharness='pi'\n+++\nSECRET role-only\n",
        )
        .unwrap();
        let registry = spawn_registry();
        registry
            .register(atm_core::SessionDomain {
                id: "session-%2".into(),
                tmux_pane: Some("%2".into()),
                harness: atm_core::Harness::Pi,
                status: atm_core::SessionStatus::AttentionNeeded,
                first_prompt: Some("SECRET session prompt".into()),
                last_activity: now(),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut services = MethodServices::new(registry.clone());
        services.org = org.clone();
        Self {
            dir,
            org,
            registry,
            methods: Arc::new(router(services).unwrap()),
            cancel,
        }
    }
    async fn call(
        &self,
        pane: Option<&str>,
        params: Value,
    ) -> Result<Value, atmd::server::methods::MethodError> {
        self.methods
            .call(
                CallContext {
                    client_id: CallerId("tree-test".into()),
                    from_pane: pane.map(|p| CallerPane(p.into())),
                },
                "org.tree",
                params,
            )
            .await
    }
    async fn seed_private(&self) {
        for command in [
            OrgCommand::SendMessage(atm_org::MessageContent {delivery_notice:None,from:Address::Owner,to:"alpha/worker".parse().unwrap(),kind:atm_org::MessageKind::Brief,body:"SECRET message body".into(),attachments:vec!["SECRET message attachment".into()],task:None}),
            OrgCommand::Decision(atm_org::DecisionCommand::Request(serde_json::from_value(json!({"requester":"alpha/worker","kind":"plan","title":"SECRET decision title","context":"SECRET decision context","files":["SECRET decision file"],"options":["yes","no"],"recommended":"yes","may_be_undone":true,"changes_intent":true})).unwrap())),
            OrgCommand::Task(atm_org::TaskCommand::Create(serde_json::from_value(json!({"title":"Public task title","kind":"work","creator":"alpha/worker","parent":null,"judges":null,"measure":{"kind":"mechanical","command":"SECRET task command"},"budget":null,"files":[{"path":"SECRET task file","kind":"brief","note":"SECRET task note"}],"rests_on":null})).unwrap())),
        ] {self.org.execute(command,now()).await.unwrap();}
    }
}
fn now() -> chrono::DateTime<chrono::Utc> {
    "2026-01-15T12:00:00Z".parse().unwrap()
}
fn normalized(mut value: Value) -> Value {
    value.as_object_mut().unwrap().remove("generated_at");
    value
}

// Break caught: tree task totals follow beta's worker rather than alpha's recorded intent.
#[tokio::test]
async fn tree_uses_recorded_project_but_seats_keep_their_actual_assignments() {
    let rig = Rig::new(3).await;
    let created = rig.org.execute(OrgCommand::Task(atm_org::TaskCommand::Create(serde_json::from_value(json!({"title":"Targeted", "project_id":"alpha", "kind":"work", "creator":"owner", "measure":{"kind":"owner"}, "files":[]})).unwrap())), now()).await.unwrap();
    let id: atm_org::TaskId = format!("t{}", created[0].seq).parse().unwrap();
    rig.org
        .execute(
            OrgCommand::Task(atm_org::TaskCommand::Assign {
                task_id: id.clone(),
                agent_id: "beta/broken".parse().unwrap(),
                by: atm_org::TaskActor {
                    who: Address::Owner,
                    manager_of: None,
                },
            }),
            now(),
        )
        .await
        .unwrap();
    let assigned = rig.call(None, json!({})).await.unwrap();
    assert_eq!(assigned["projects"][0]["tasks_open"], 1);
    assert_eq!(assigned["projects"][1]["tasks_open"], 0);
    assert_eq!(assigned["projects"][1]["seats"][0]["tasks"], json!([id]));
    rig.org
        .execute(
            OrgCommand::Task(atm_org::TaskCommand::SetState {
                task_id: id.clone(),
                state: atm_org::TaskState::Open,
                reason: None,
                by: atm_org::TaskActor {
                    who: Address::Owner,
                    manager_of: None,
                },
            }),
            now(),
        )
        .await
        .unwrap();
    let reopened = rig.call(None, json!({})).await.unwrap();
    assert_eq!(reopened["projects"][0]["tasks_open"], 1);
    assert!(reopened["projects"][1]["seats"][0]["tasks"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(reopened["unplaced_tasks"].as_array().unwrap().is_empty());
}

// Break caught: method not advertised, accepts unknown/scalar params, depends
// on caller authority or exposes private source content through serialization.
#[tokio::test]
async fn org_tree_advertises_strict_params_and_no_new_visibility() {
    for count in [3, 5] {
        let rig = Rig::new(count).await;
        rig.seed_private().await;
        let info = rig
            .methods
            .call(
                CallContext {
                    client_id: CallerId("owner".into()),
                    from_pane: None,
                },
                "daemon.info",
                json!({}),
            )
            .await
            .unwrap();
        assert!(info["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("org.tree")));
        let expected = normalized(rig.call(None, json!({})).await.unwrap());
        assert!(!expected.to_string().contains("SECRET"));
        for pane in [
            None,
            Some("%1"),
            Some("%2"),
            Some("%4"),
            Some("%99"),
            Some("not-a-pane"),
        ] {
            assert_eq!(
                normalized(rig.call(pane, Value::Null).await.unwrap()),
                expected
            );
        }
        assert_eq!(expected["projects"].as_array().unwrap().len(), count);
        assert_eq!(
            expected["projects"][0]["seats"].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            expected["projects"][0]["seats"][0]["manager"],
            "gamma/chief"
        );
        assert_eq!(expected["projects"][0]["seats"][1]["manager"], "alpha/lead");
        assert_eq!(
            expected["projects"][1]["seats"][0]["line_error"],
            "reporting line at beta/broken: invalid role configuration"
        );
        assert_eq!(expected["projects"][0]["tasks_open"], 1);
        assert_eq!(
            expected["projects"][0]["decisions"],
            json!({"pending":1,"provisional":0})
        );
        for params in [
            json!({"project":"alpha"}),
            json!({"for":"owner"}),
            json!(1),
            json!([]),
            json!("text"),
        ] {
            assert_eq!(
                rig.call(None, params).await.unwrap_err().0.code,
                "invalid_params"
            );
        }
    }
}

// Break caught: connection changes the envelope, drops seq/metadata, mutates
// the fold on a read or refuses a caller without a named pane.
#[tokio::test]
async fn org_tree_exact_request_response_over_real_connection() {
    let rig = Rig::new(3).await;
    let expected = rig.call(None, json!({})).await.unwrap();
    let (ours, peer) = tokio::io::duplex(1024 * 1024);
    let (read, write) = tokio::io::split(ours);
    let cancel = CancellationToken::new();
    let handler = ConnectionHandler::new(
        read,
        write,
        rig.registry.clone(),
        Arc::new(RwLock::new(HashMap::new())),
        1,
        cancel.clone(),
        rig.methods.clone(),
    );
    let task = tokio::spawn(handler.run());
    let (read, mut write) = tokio::io::split(peer);
    let mut read = BufReader::new(read);
    write
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&ClientMessage::connect(Some("tree-client".into()))).unwrap()
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
        .contains(&json!("org.tree")));
    let request = json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"tree-1","method":"org.tree","params":{},"from_pane":"%2"});
    write
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    line.clear();
    tokio::time::timeout(Duration::from_secs(60), read.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        response
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["id", "result", "type"]
    );
    assert_eq!(response["id"], "tree-1");
    assert_eq!(response["type"], "response");
    assert_eq!(normalized(response["result"].clone()), normalized(expected));
    assert_eq!(rig.org.status().unwrap(), 7);
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .unwrap()
        .unwrap();
}

// Break caught: reporting edges are cached between calls or a broken line
// hides healthy independent seats when a current role changes.
#[tokio::test]
async fn org_tree_fresh_roles_after_configuration_edits() {
    let rig = Rig::new(3).await;
    let first = rig.call(None, json!({})).await.unwrap();
    assert_eq!(first["projects"][0]["seats"][1]["manager"], "alpha/lead");
    std::fs::remove_file(rig.dir.path().join("alpha/.atm/roles/worker.md")).unwrap();
    std::fs::write(
        rig.dir.path().join("beta/.atm/roles/broken.md"),
        "+++\nharness='pi'\n+++\nFresh private instructions\n",
    )
    .unwrap();
    let next = rig.call(None, json!({})).await.unwrap();
    assert_eq!(next["seq"], first["seq"]);
    assert_eq!(next["projects"][0]["seats"][1]["manager"], "owner");
    assert_eq!(next["projects"][1]["seats"][0]["manager"], "owner");
    assert!(next["projects"][1]["seats"][0]["line_error"].is_null());
}

// Break caught: channel loss masquerades as a successful empty registry/fold.
#[tokio::test]
async fn org_tree_unavailable_services_fail_explicitly() {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    let (events, _) = tokio::sync::broadcast::channel(1);
    let mut services = MethodServices::new(RegistryHandle::new(tx, events));
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    services.org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let methods = router(services).unwrap();
    let context = CallContext {
        client_id: CallerId("unbound".into()),
        from_pane: None,
    };
    assert_eq!(
        methods
            .call(context.clone(), "org.tree", json!({}))
            .await
            .unwrap_err()
            .0
            .code,
        "unavailable"
    );
    let methods = router(MethodServices::new(spawn_registry())).unwrap();
    assert_eq!(
        methods
            .call(context, "org.tree", json!({}))
            .await
            .unwrap_err()
            .0
            .code,
        "unavailable"
    );
    cancel.cancel();
}
