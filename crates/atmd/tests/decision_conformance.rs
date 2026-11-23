//! Decision methods on a private real server; synthetic org state, no harness.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use atm_org::OrgCommand;
use atm_protocol::{ClientMessage, DaemonMessage, ResponsePayload};
use atmd::{
    org::{agents::AgentHandle, messages::MessageHandle, OrgHandle},
    registry::spawn_registry,
    server::{methods::MethodServices, DaemonServer},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tokio_util::sync::CancellationToken;

struct Rig {
    dir: tempfile::TempDir,
    org: OrgHandle,
    org_cancel: CancellationToken,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), atmd::server::ServerError>>,
}
impl Rig {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let org_cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), org_cancel.clone()).await;
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
            OrgCommand::Adopt {
                agent_id: "project/other".parse().unwrap(),
                pane: "%8".into(),
                session_id: None,
            },
        ] {
            org.execute(command, chrono::Utc::now()).await.unwrap();
        }
        let cancel = CancellationToken::new();
        let mut services = MethodServices::new(spawn_registry());
        services.org = org.clone();
        services.agents = Some(AgentHandle::disabled("fixture"));
        services.messages = Some(MessageHandle::disabled("fixture"));
        let (control, _) = atmd::control::service::spawn_control(
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
        let task = tokio::spawn(run);
        Self {
            dir,
            org,
            org_cancel,
            cancel,
            task,
        }
    }
    async fn call(
        &self,
        method: &str,
        params: Value,
        pane: Option<&str>,
    ) -> Result<Value, atm_protocol::ResponseError> {
        tokio::time::timeout(Duration::from_secs(60), async {
            let stream = UnixStream::connect(self.dir.path().join("atm.sock"))
                .await
                .unwrap();
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let connect = ClientMessage::connect(Some("decision-fixture".into()));
            write
                .write_all(format!("{}\n", serde_json::to_string(&connect).unwrap()).as_bytes())
                .await
                .unwrap();
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["type"],
                "connected"
            );
            let request = ClientMessage::request(
                "decision".into(),
                method.into(),
                params,
                pane.map(str::to_owned),
            );
            write
                .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
                .await
                .unwrap();
            line.clear();
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
    async fn request(&self) -> Value {
        self.call("decision.request", request(), Some("%7"))
            .await
            .unwrap()
    }
    async fn stop(self) {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(60), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.org_cancel.cancel();
    }
}
fn request() -> Value {
    json!({"kind":"plan","title":"Approve this plan","context":"Choose the smaller shape","files":["plan.md"],"options":["yes","no"],"recommended":"yes","may_be_undone":true,"changes_intent":true,"deadline":null})
}

// Break caught: absent methods, caller-supplied attribution, or a bound agent
// answering for the owner. The complete method fixtures extend this flow below.
#[tokio::test]
async fn decision_methods_request_read_resolve_and_repeat() {
    let rig = Rig::new().await;
    assert_eq!(
        rig.request().await,
        json!({"decision_id":"d4","post_id":"m5"})
    );
    let list = rig.call("decision.list", json!({}), None).await.unwrap();
    assert_eq!(list["pending_count"], 1);
    assert_eq!(list["provisional_count"], 0);
    assert_eq!(
        list["decisions"][0]["request"]["requester"],
        "project/worker"
    );
    let got = rig
        .call("decision.get", json!({"id":"d4"}), Some("%7"))
        .await
        .unwrap();
    assert_eq!(got["decision"], list["decisions"][0]);
    assert_eq!(
        rig.call(
            "decision.resolve",
            json!({"id":"d4","option":"yes"}),
            Some("%7")
        )
        .await
        .unwrap_err()
        .code,
        "refused"
    );
    let mut events = rig
        .org
        .watch(vec!["decision.resolved".into()], rig.org.status().unwrap())
        .unwrap();
    let mut answers = rig
        .org
        .watch(vec!["msg.sent".into()], rig.org.status().unwrap())
        .unwrap();
    let params = json!({"id":"d4","option":"yes","note":"owner note","standing_rule":true});
    let answer = rig
        .call("decision.resolve", params.clone(), Some("%9"))
        .await
        .unwrap();
    assert_eq!(answer["changed"], true);
    assert_eq!(answer["decision"]["status"], "resolved");
    let audit = tokio::time::timeout(Duration::from_secs(60), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(audit.body["caller_pane"], "%9");
    assert_eq!(audit.body["by"], "owner");
    // The question's row to the owner (U8) is the bridge's to write after the
    // request returned, so it may come first: pass over it.
    let message = loop {
        let row = tokio::time::timeout(Duration::from_secs(60), answers.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if row.body["kind"] != "question" {
            break row;
        }
    };
    // U8: the answer is a reply's wake row: today's message plus `post`.
    let mut body = message.body.clone();
    assert!(body.as_object_mut().unwrap().remove("post").is_some());
    assert_eq!(
        body,
        json!({"from":"system","to":"project/worker","kind":"answer","body":"Decision d4 resolved: yes\nNote: owner note\nStanding rule: yes","attachments":[],"task":null})
    );
    let repeat = rig
        .call("decision.resolve", params, Some("%10"))
        .await
        .unwrap();
    assert_eq!(repeat["changed"], false);
    assert_eq!(repeat["decision"], answer["decision"]);
    // One answer; the other row is the question's to the owner (U8).
    let state = rig.org.snapshot().await.unwrap();
    let kinds: Vec<_> = state.messages().map(|m| m.content.kind).collect();
    assert_eq!(
        kinds,
        [atm_org::MessageKind::Question, atm_org::MessageKind::Answer]
    );
    assert_eq!(
        rig.call("decision.list", json!({"count_only":true}), Some("%8"))
            .await
            .unwrap(),
        json!({"decisions":[],"pending_count":0,"provisional_count":0,"next_after":null})
    );
    rig.stop().await;
}

#[tokio::test]
async fn decision_methods_withdraw_permissions_and_counts() {
    let rig = Rig::new().await;
    rig.request().await;
    assert_eq!(
        rig.call("decision.get", json!({"id":"d999"}), None)
            .await
            .unwrap_err()
            .code,
        "not_found"
    );
    for method in ["decision.get", "decision.withdraw"] {
        assert_eq!(
            rig.call(method, json!({"id":"d4"}), Some("%8"))
                .await
                .unwrap_err()
                .code,
            "refused"
        );
    }
    assert_eq!(
        rig.call("decision.list", json!({}), Some("%8"))
            .await
            .unwrap()["pending_count"],
        0
    );
    assert_eq!(
        rig.call("decision.list", json!({"count_only":true}), Some("%8"))
            .await
            .unwrap()["pending_count"],
        1
    );
    assert_eq!(
        rig.call(
            "decision.withdraw",
            json!({"id":"d4","reason":"not needed"}),
            Some("%7")
        )
        .await
        .unwrap()["decision"]["status"],
        "withdrawn"
    );
    assert_eq!(
        rig.call("decision.withdraw", json!({"id":"d4"}), None)
            .await
            .unwrap()["changed"],
        false
    );
    rig.stop().await;
}

fn normalize_times(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(key.as_str(), "requested_at" | "closed_at" | "taken_at")
                    && !value.is_null()
                {
                    value
                        .as_str()
                        .unwrap()
                        .parse::<chrono::DateTime<chrono::Utc>>()
                        .unwrap();
                    *value = json!("<timestamp>");
                } else {
                    normalize_times(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                normalize_times(value);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn decision_method_success_and_refusal_golden_fixtures() {
    let fixtures: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/decisions/methods.json")).unwrap();
    let mut coverage = std::collections::BTreeMap::<String, (bool, bool)>::new();
    for fixture in fixtures {
        let rig = Rig::new().await;
        if fixture["seed"] == true {
            rig.request().await;
        }
        let method = fixture["method"].as_str().unwrap();
        let actual = rig
            .call(method, fixture["params"].clone(), fixture["pane"].as_str())
            .await;
        let seen = coverage.entry(method.into()).or_default();
        if let Some(expected) = fixture.get("error") {
            let error = actual.unwrap_err();
            assert_eq!(serde_json::to_value(error).unwrap(), *expected, "{method}");
            seen.1 = true;
        } else {
            let mut actual = actual.unwrap();
            normalize_times(&mut actual);
            assert_eq!(actual, fixture["result"], "{method}");
            seen.0 = true;
        }
        rig.stop().await;
    }
    assert_eq!(coverage.len(), 5);
    assert!(coverage.values().all(|seen| *seen == (true, true)));
}

// Break caught: a replay-only decision kind bypasses RPC validation and reaches policy.
#[tokio::test]
async fn unknown_decision_request_kind_is_invalid_params_without_writes() {
    let rig = Rig::new().await;
    let disabled = atmd::server::methods::router(MethodServices::new(spawn_registry())).unwrap();
    let before = rig.org.snapshot().await.unwrap();
    let cursor = rig.org.status().unwrap();
    for word in ["bogus", "unknown"] {
        let mut params = request();
        params["kind"] = json!(word);
        let result = rig
            .call("decision.request", params.clone(), Some("%7"))
            .await;
        assert_eq!(rig.org.snapshot().await.unwrap(), before);
        assert_eq!(rig.org.status().unwrap(), cursor);
        let error = result.unwrap_err();
        assert_eq!(error.code, "invalid_params", "{params}: {error:?}");
        assert!(error.message.contains("kind"), "{error:?}");
        let error = disabled
            .call(
                atmd::server::methods::CallContext {
                    client_id: atmd::server::methods::CallerId("unknown-kind".into()),
                    from_pane: None,
                },
                "decision.request",
                params,
            )
            .await
            .unwrap_err()
            .0;
        assert_eq!(error.code, "invalid_params");
        assert!(error.message.contains("kind"));
    }
    assert_eq!(
        rig.request().await,
        json!({"decision_id":"d4","post_id":"m5"})
    );
    rig.stop().await;
}

#[tokio::test]
async fn decision_method_schemas_and_semantic_refusals() {
    let rig = Rig::new().await;
    let mut bad_requests = Vec::new();
    for (field, value) in [
        ("kind", json!("merge")),
        ("kind", json!("unknown")),
        ("title", json!("")),
        ("title", json!("é".repeat(257))),
        ("context", json!("x".repeat(16385))),
        ("options", json!(["yes"])),
        ("options", json!(["yes", "yes"])),
        ("options", json!(["", "no"])),
        ("options", json!(["x".repeat(1025), "no"])),
        ("recommended", json!("missing")),
        ("deadline", json!("2000-01-01T00:00:00Z")),
        ("files", json!([""])),
        ("files", json!(["x".repeat(4097)])),
        ("files", json!(vec!["file"; 33])),
        ("requester", json!("owner")),
        ("caller_pane", json!("%9")),
    ] {
        let mut params = request();
        params[field] = value;
        bad_requests.push(params);
    }
    for params in bad_requests {
        assert_eq!(
            rig.call("decision.request", params.clone(), Some("%7"))
                .await
                .unwrap_err()
                .code,
            "invalid_params",
            "{params}"
        );
    }
    for pane in [None, Some("%9")] {
        assert_eq!(
            rig.call("decision.request", request(), pane)
                .await
                .unwrap_err()
                .code,
            "refused"
        );
    }
    for params in [
        json!({"limit":0}),
        json!({"limit":1001}),
        json!({"statuses":[]}),
        json!({"statuses":["future"]}),
        json!({"count_only":true,"after":"d4"}),
        json!({"count_only":true,"statuses":["pending"]}),
        json!({"typo":true}),
        json!([]),
    ] {
        assert_eq!(
            rig.call("decision.list", params.clone(), None)
                .await
                .unwrap_err()
                .code,
            "invalid_params",
            "{params}"
        );
    }
    for pane in ["%07", "7", "%", "%7\n"] {
        assert_eq!(
            rig.call("decision.list", json!({}), Some(pane))
                .await
                .unwrap_err()
                .code,
            "invalid_params"
        );
    }
    for id in ["d01", "1", "D1", "m1", "d18446744073709551616"] {
        for method in ["decision.get", "decision.resolve", "decision.withdraw"] {
            let params = if method == "decision.resolve" {
                json!({"id":id,"option":"yes"})
            } else {
                json!({"id":id})
            };
            assert_eq!(
                rig.call(method, params, None).await.unwrap_err().code,
                "invalid_params"
            );
        }
    }
    rig.request().await;
    for params in [
        json!({"id":"d4","option":"missing"}),
        json!({"id":"d4","option":"yes","by":"owner"}),
        json!({"id":"d4","option":"yes","caller_pane":"%9"}),
        json!({"id":"d4","option":"yes","note":"x".repeat(8193)}),
    ] {
        let expected = if params["option"] == "missing" {
            "refused"
        } else {
            "invalid_params"
        };
        assert_eq!(
            rig.call("decision.resolve", params, None)
                .await
                .unwrap_err()
                .code,
            expected
        );
    }
    rig.call("decision.resolve", json!({"id":"d4","option":"yes"}), None)
        .await
        .unwrap();
    for params in [
        json!({"id":"d4","option":"no"}),
        json!({"id":"d4","option":"yes","standing_rule":true}),
        json!({"id":"d4","option":"yes","note":"new note"}),
    ] {
        assert_eq!(
            rig.call("decision.resolve", params, None)
                .await
                .unwrap_err()
                .code,
            "refused"
        );
    }
    rig.stop().await;
}

#[tokio::test]
async fn decision_lists_count_all_pages_and_keep_provisional_answers_visible() {
    let rig = Rig::new().await;
    for _ in 0..101 {
        rig.request().await;
    }
    let page = rig.call("decision.list", json!({}), None).await.unwrap();
    assert_eq!(page["decisions"].as_array().unwrap().len(), 100);
    assert_eq!(page["pending_count"], 101);
    // U8: each request is three records: the decision, its question post and
    // the owner's row of it.
    assert_eq!(page["decisions"][0]["id"], "d4");
    assert_eq!(page["decisions"][99]["id"], "d301");
    assert_eq!(page["next_after"], "d301");
    let tail = rig
        .call("decision.list", json!({"after":"d301"}), None)
        .await
        .unwrap();
    assert_eq!(tail["decisions"].as_array().unwrap().len(), 1);
    assert_eq!(tail["decisions"][0]["id"], "d304");
    assert_eq!(tail["next_after"], Value::Null);
    rig.org
        .execute(
            OrgCommand::Decision(atm_org::DecisionCommand::TakeProvisionally(
                atm_org::ProvisionalChoice {
                    relayed_by: None,
                    decision_id: atm_org::DecisionId::new(4),
                    option: "yes".into(),
                    by: atm_org::Address::System,
                },
            )),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    let counts = rig
        .call("decision.list", json!({"count_only":true}), Some("%8"))
        .await
        .unwrap();
    assert_eq!(
        counts,
        json!({"decisions":[],"pending_count":100,"provisional_count":1,"next_after":null})
    );
    let mut answers = rig
        .org
        .watch(vec!["msg.sent".into()], rig.org.status().unwrap())
        .unwrap();
    let answer = rig
        .call("decision.resolve", json!({"id":"d4","option":"no"}), None)
        .await
        .unwrap();
    assert_eq!(answer["decision"]["status"], "overruled");
    // The take's own notice, and question rows to the owner, may still arrive
    // first: wait for the answer.
    let message = loop {
        let row = tokio::time::timeout(Duration::from_secs(60), answers.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if row.body["kind"] != "question"
            && !row.body["body"]
                .as_str()
                .unwrap()
                .starts_with("Decision d4 taken provisionally")
        {
            break row;
        }
    };
    // U8: the answer is a reply's wake row: today's message plus `post`.
    let mut body = message.body.clone();
    assert!(body.as_object_mut().unwrap().remove("post").is_some());
    assert_eq!(
        body,
        json!({"from":"system","to":"project/worker","kind":"answer","body":"Decision d4 overruled: no\nStanding rule: no","attachments":[],"task":null})
    );
    assert_eq!(
        rig.call("decision.list", json!({"statuses":["overruled"]}), None)
            .await
            .unwrap()["decisions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    rig.stop().await;
}

// U8 (design 12): a decision and its question post name each other. Break
// caught: rows without the link, a link to the wrong post or channel, or a
// link only on some methods.
#[tokio::test]
async fn decision_request_returns_its_question_post_and_rows_link_it() {
    let rig = Rig::new().await;
    let made = rig.request().await;
    let post = made["post_id"].as_str().unwrap().to_owned();
    // `project` has no lead: the question is in the asker's DM with the owner.
    let channel = "project/worker+owner";
    let state = rig.org.snapshot().await.unwrap();
    let asked = atm_org::channel_query::post(&state, post.parse().unwrap()).unwrap();
    assert_eq!(asked.content.channel.to_string(), channel);
    let linked = |row: &Value| {
        assert_eq!(row["post_id"], post, "{row}");
        assert_eq!(row["channel"], channel, "{row}");
    };
    linked(&rig.call("decision.list", json!({}), None).await.unwrap()["decisions"][0]);
    linked(
        &rig.call("decision.get", json!({"id":"d4"}), None)
            .await
            .unwrap()["decision"],
    );
    linked(
        &rig.call(
            "decision.withdraw",
            json!({"id":"d4","reason":"not needed"}),
            Some("%7"),
        )
        .await
        .unwrap()["decision"],
    );
    let next = rig.request().await;
    let resolved = rig
        .call(
            "decision.resolve",
            json!({"id":next["decision_id"],"option":"yes"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(resolved["decision"]["post_id"], next["post_id"]);
    assert_eq!(resolved["decision"]["channel"], channel);
    rig.stop().await;
}

// The bridge relays the owner's button as a call with no pane (W0). Break
// caught: such a caller treated as anyone but the owner.
#[tokio::test]
async fn decision_resolve_from_a_socket_without_a_pane_is_the_owners_answer() {
    let rig = Rig::new().await;
    rig.request().await;
    let answer = rig
        .call("decision.resolve", json!({"id":"d4","option":"no"}), None)
        .await
        .unwrap();
    assert_eq!(answer["changed"], true);
    assert_eq!(answer["decision"]["status"], "resolved");
    assert_eq!(answer["decision"]["resolution"]["by"], "owner");
    rig.stop().await;
}
