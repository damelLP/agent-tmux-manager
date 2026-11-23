//! Wire-level conformance cases, replayed against a real private server.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use atm_protocol::{ClientMessage, DaemonMessage};
use atmd::{
    registry::spawn_registry,
    server::{
        methods::{router, MethodServices},
        DaemonServer,
    },
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{unix::OwnedReadHalf, unix::OwnedWriteHalf, UnixStream};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[allow(dead_code)]
#[path = "../src/control/legacy_survey_test.rs"]
mod survey_viewport;

struct Server {
    socket: PathBuf,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), atmd::server::ServerError>>,
    _dir: tempfile::TempDir,
}

impl Server {
    async fn start() -> Self {
        Self::with_services(MethodServices::new(spawn_registry())).await
    }

    async fn with_services(services: MethodServices) -> Self {
        Self::with_fixture_request(services, None).await
    }

    async fn with_fixture_request(mut services: MethodServices, request: Option<&Value>) -> Self {
        if services.control.is_none() {
            let mock = atm_tmux::MockTmuxClient::new();
            let method = request.and_then(|r| r["method"].as_str());
            if method != Some("session.reply") {
                mock.set_next_raw_output("0 1\n");
            }
            if matches!(method, Some("session.send" | "session.reply")) {
                survey_viewport::mock_shell(&mock, "%7");
            }
            mock.set_next_pane_id("%42");
            mock.set_pane_cwd("%7", "/from/pane");
            let (control, _) = atmd::control::service::spawn_control(
                Box::new(mock),
                services.registry.clone(),
                services.org.clone(),
                CancellationToken::new(),
            );
            services.control = Some(control);
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let cancel = CancellationToken::new();
        let server = DaemonServer::with_services(&socket, services, cancel.clone());
        let mut run = Box::pin(async move { server.run().await });
        // run binds synchronously before its first Pending accept.
        std::future::poll_fn(|cx| {
            use std::{future::Future, task::Poll};
            assert!(run.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        use tracing::instrument::WithSubscriber;
        let task = tokio::spawn(run.with_current_subscriber());
        Self {
            socket,
            cancel,
            task,
            _dir: dir,
        }
    }

    async fn client(&self) -> Client {
        let stream = timeout(Duration::from_secs(5), UnixStream::connect(&self.socket))
            .await
            .unwrap()
            .unwrap();
        let (read, write) = stream.into_split();
        Client {
            read: BufReader::new(read),
            write,
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

struct Client {
    read: BufReader<OwnedReadHalf>,
    write: OwnedWriteHalf,
}
impl Client {
    async fn send(&mut self, value: Value) {
        timeout(
            Duration::from_secs(5),
            self.write.write_all(format!("{value}\n").as_bytes()),
        )
        .await
        .unwrap()
        .unwrap();
    }
    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        let n = timeout(Duration::from_secs(5), self.read.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(n, 0, "unexpected EOF");
        println!("wire: {}", line.trim());
        serde_json::from_str(&line).unwrap()
    }
    async fn require_next_pong(&mut self) {
        self.send(serde_json::to_value(ClientMessage::ping(987)).unwrap())
            .await;
        assert_eq!(
            self.recv().await,
            json!({"type":"pong","seq":987}),
            "fixture emitted an extra reply"
        );
    }

    async fn connect(&mut self) -> Value {
        self.send(json!({"protocol_version":{"major":1,"minor":0},"type":"connect","client_id":"conformance"})).await;
        self.recv().await
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    // Legacy fixtures (including caller-server transport cases) test the
    // disabled contract unless they explicitly opt into the enabled service.
    #[serde(default)]
    agents_enabled: bool,
    request: Value,
    response: Value,
    ignore: Vec<String>,
}

fn files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "json") {
            found.push(path);
        }
    }
}

fn compare(mut actual: Value, mut expected: Value, ignore: &[String]) -> Result<(), String> {
    for pointer in ignore {
        if pointer.is_empty() {
            return Err("cannot ignore the root".into());
        }
        let a = actual
            .pointer_mut(pointer)
            .ok_or_else(|| format!("missing actual pointer {pointer}"))?;
        let e = expected
            .pointer_mut(pointer)
            .ok_or_else(|| format!("missing expected pointer {pointer}"))?;
        *a = Value::Null;
        *e = Value::Null;
    }
    if actual == expected {
        Ok(())
    } else {
        Err(format!("actual: {actual}\nexpected: {expected}"))
    }
}

#[test]
fn ignore_pointers_do_not_hide_structure_errors() {
    let a = json!({"a/b":{"~key":1},"keep":true});
    let b = json!({"a/b":{"~key":2},"keep":true});
    assert!(compare(a.clone(), b.clone(), &["/a~1b/~0key".into()]).is_ok());
    assert!(compare(a.clone(), b.clone(), &["/missing".into()]).is_err());
    assert!(compare(a.clone(), b.clone(), &[String::new()]).is_err());
    assert!(compare(a, json!({"a/b":{}}), &["/a~1b/~0key".into()]).is_err());
    assert!(compare(json!({"extra":1}), json!({}), &[]).is_err());
}

async fn replay_fixture(server: &Server, request: Value) -> (Value, Value) {
    let mut client = server.client().await;
    let connected = client.connect().await;
    client.send(request).await;
    let response = client.recv().await;
    client.require_next_pong().await;
    (connected, response)
}

// The new names must be advertised and have the same wire contract as the old names.
#[tokio::test]
async fn replay_all_protocol_fixtures() {
    let wake_pairs = [
        ("msg.inbox", "wake.list"),
        ("msg.ack", "wake.ack"),
        ("msg.retry", "wake.retry"),
        ("msg.give_up", "wake.give_up"),
    ];
    let mut covered = std::collections::BTreeSet::new();
    let mut paths = Vec::new();
    files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/protocol"),
        &mut paths,
    );
    paths.sort();
    assert!(!paths.is_empty());
    for path in paths {
        let fixture: Fixture = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let state = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let mut services = MethodServices::new(spawn_registry());
        services.state_dir = state.path().into();
        services.org = atmd::org::OrgHandle::open(state.path().into(), cancel.clone()).await;
        if !fixture.agents_enabled {
            services.agents = Some(atmd::org::agents::AgentHandle::disabled(
                "named-agent operations are not configured",
            ));
        }
        let mut request = fixture.request;
        let server = Server::with_fixture_request(services, Some(&request)).await;
        let (connected, actual) = replay_fixture(&server, request.clone()).await;
        let alias = if let Some(&(old, new)) =
            wake_pairs.iter().find(|(old, _)| request["method"] == *old)
        {
            // These fixtures stop at validation or the disabled message service.
            request["method"] = json!(new);
            Some((old, new, replay_fixture(&server, request).await))
        } else {
            None
        };
        server.stop().await;
        cancel.cancel();
        let compared = compare(actual, fixture.response.clone(), &fixture.ignore);
        assert!(compared.is_ok(), "{}: {compared:?}", path.display());
        if let Some((old, new, (alias_connected, reply))) = alias {
            assert!(fixture.ignore.is_empty(), "{}: {new}", path.display());
            assert_eq!(reply, fixture.response, "{}: {new}", path.display());
            for hello in [connected, alias_connected] {
                let methods = hello["methods"].as_array().unwrap();
                for method in [old, new] {
                    assert!(
                        methods.iter().any(|name| name == method),
                        "missing {method}"
                    );
                }
            }
            covered.insert(new);
        }
    }
    assert_eq!(covered.len(), wake_pairs.len(), "missing alias fixtures");
}

#[tokio::test]
async fn spawn_prepare_largest_conforms_on_real_socket() {
    use atm_tmux::{mock::MockCall, PaneInfo};
    let state = tempfile::tempdir().unwrap();
    let own = state.path().join("tmux.sock");
    let foreign = state.path().join("foreign.sock");
    std::fs::write(&own, "").unwrap();
    std::fs::write(&foreign, "").unwrap();
    let mock = atm_tmux::MockTmuxClient::new();
    let mut services = MethodServices::new(spawn_registry());
    let cancel = CancellationToken::new();
    let (control, actor) = atmd::control::service::spawn_control(
        Box::new(mock.clone()),
        services.registry.clone(),
        services.org.clone(),
        cancel.clone(),
    );
    services.control = Some(control);
    let server = Server::with_services(services).await;
    let mut client = server.client().await;
    let connected = client.connect().await;
    assert!(connected["methods"]
        .as_array()
        .unwrap()
        .contains(&json!("session.spawn_prepare_largest")));
    for (panes, caller, cwd, expected) in [
        (
            vec![],
            None,
            None,
            Some(json!({"target_pane":"%0","cwd":null})),
        ),
        (
            vec![],
            Some("%999"),
            None,
            Some(json!({"target_pane":"%999","cwd":null})),
        ),
        (
            vec![PaneInfo {
                pane_id: "%7".into(),
                session_name: "work".into(),
                window_index: 1,
                pane_pid: 1,
                width: 100,
                height: 60,
                is_active: false,
            }],
            None,
            Some("/selected".into()),
            Some(json!({"target_pane":"%7","cwd":"/selected"})),
        ),
        (
            vec![PaneInfo {
                pane_id: "%7".into(),
                session_name: "work".into(),
                window_index: 1,
                pane_pid: 1,
                width: 100,
                height: 60,
                is_active: false,
            }],
            None,
            Some("x".repeat(1_048_576)),
            None,
        ),
    ] {
        mock.set_panes(panes);
        if let Some(cwd) = cwd {
            mock.set_pane_cwd("%7", &cwd);
        }
        mock.set_next_raw_output(own.to_str().unwrap());
        client.send(json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"largest","method":"session.spawn_prepare_largest","params":{"caller_server":own,"caller_pane":caller},"from_pane":"%caller"})).await;
        let response = client.recv().await;
        if let Some(expected) = expected {
            assert_eq!(response["result"], expected);
        } else {
            assert_eq!(response["error"]["code"], "refused");
        }
        assert!(serde_json::to_vec(&response).unwrap().len() < 1_048_576);
        client.require_next_pong().await;
    }
    mock.clear_calls();
    mock.set_next_raw_output(own.to_str().unwrap());
    client.send(json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"foreign","method":"session.spawn_prepare_largest","params":{"caller_server":foreign,"caller_pane":"%7"}})).await;
    assert_eq!(client.recv().await["error"]["code"], "refused");
    assert_eq!(
        mock.calls(),
        vec![MockCall::RawCommand {
            subcommand: "display-message".into(),
            args: vec!["-p".into(), "#{socket_path}".into()]
        }]
    );
    client.require_next_pong().await;
    server.stop().await;
    cancel.cancel();
    actor.await.unwrap();
}

// Breaks if the advertised membership schema or full-frame refusal differs
// on a real private socket (not just in the method/duplex unit tests).
#[tokio::test]
async fn session_panes_conforms_on_real_socket_including_oversize() {
    use atm_tmux::PaneInfo;
    let state = tempfile::tempdir().unwrap();
    let own = state.path().join("tmux.sock");
    std::fs::write(&own, "").unwrap();
    let mock = atm_tmux::MockTmuxClient::new();
    let pane = |id: String, name: &str| PaneInfo {
        pane_id: id,
        session_name: name.into(),
        window_index: 0,
        pane_pid: 1,
        width: 80,
        height: 24,
        is_active: true,
    };
    let mut services = MethodServices::new(spawn_registry());
    let cancel = CancellationToken::new();
    let (control, actor) = atmd::control::service::spawn_control(
        Box::new(mock.clone()),
        services.registry.clone(),
        services.org.clone(),
        cancel.clone(),
    );
    services.control = Some(control);
    let server = Server::with_services(services).await;
    let mut client = server.client().await;
    let connected = client.connect().await;
    assert!(connected["methods"]
        .as_array()
        .unwrap()
        .iter()
        .any(|name| name == "session.panes"));
    for (panes, expected) in [
        (
            vec![
                pane("%2".into(), "work"),
                pane("%1".into(), "work-extra"),
                pane("%0".into(), "work"),
            ],
            Some(json!({"pane_ids":["%0","%2"]})),
        ),
        (
            vec![pane("%1".into(), "work-extra")],
            Some(json!({"pane_ids":[]})),
        ),
        (
            vec![pane(format!("%{}", "7".repeat(1_048_576)), "work")],
            None,
        ),
    ] {
        mock.set_panes(panes);
        mock.set_next_raw_output(own.to_str().unwrap());
        client
            .send(
                serde_json::to_value(ClientMessage::request(
                    "panes".into(),
                    "session.panes".into(),
                    json!({"session_name":"work","caller_server":own}),
                    Some("%9".into()),
                ))
                .unwrap(),
            )
            .await;
        let response = client.recv().await;
        if let Some(expected) = expected {
            assert_eq!(response["result"], expected);
        } else {
            assert_eq!(response["error"]["code"], "refused");
        }
        assert!(serde_json::to_vec(&response).unwrap().len() < 1_048_576);
        client.require_next_pong().await;
    }
    server.stop().await;
    cancel.cancel();
    actor.await.unwrap();
}

// Breaks if real wire role operations lose advertisement, serialize failures as
// empty reads, or leak/write role data through a foreign equal pane id.
#[tokio::test]
async fn session_roles_conform_on_real_socket_including_oversize() {
    let state = tempfile::tempdir().unwrap();
    let own = state.path().join("tmux.sock");
    let foreign = state.path().join("foreign.sock");
    std::fs::write(&own, "").unwrap();
    std::fs::write(&foreign, "").unwrap();
    let mock = atm_tmux::MockTmuxClient::new();
    let mut services = MethodServices::new(spawn_registry());
    let cancel = CancellationToken::new();
    let (control, actor) = atmd::control::service::spawn_control(
        Box::new(mock.clone()),
        services.registry.clone(),
        services.org.clone(),
        cancel.clone(),
    );
    services.control = Some(control);
    let server = Server::with_services(services).await;
    let mut client = server.client().await;
    let connected = client.connect().await;
    for method in ["session.roles", "session.role_set"] {
        assert!(connected["methods"]
            .as_array()
            .unwrap()
            .contains(&json!(method)));
    }
    for (output, expected) in [
        (
            "%7\tcoordinator\n%2\tcoordinator\n%7\tcoordinator\n".into(),
            Some(json!({"coordinator_panes":["%2","%7"]})),
        ),
        (
            "%7\tunknown\n".into(),
            Some(json!({"coordinator_panes":[]})),
        ),
        (format!("%{}\tcoordinator\n", "7".repeat(1_048_576)), None),
    ] {
        mock.set_next_raw_output(own.to_str().unwrap());
        mock.set_next_raw_output(&output);
        client.send(json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"roles","method":"session.roles","params":{"caller_server":own}})).await;
        let response = client.recv().await;
        if let Some(expected) = expected {
            assert_eq!(response["result"], expected);
        } else {
            assert_eq!(response["error"]["code"], "refused");
        }
        assert!(serde_json::to_vec(&response).unwrap().len() < 1_048_576);
        client.require_next_pong().await;
    }
    for role in [json!("coordinator"), Value::Null, Value::Null] {
        mock.set_next_raw_output(own.to_str().unwrap());
        client.send(json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"set","method":"session.role_set","params":{"caller_server":own,"pane":"%7","role":role}})).await;
        assert_eq!(
            client.recv().await["result"],
            json!({"pane":"%7","role":role})
        );
        client.require_next_pong().await;
    }
    for method in ["session.roles", "session.role_set"] {
        mock.clear_calls();
        mock.set_next_raw_output(own.to_str().unwrap());
        let params = if method == "session.roles" {
            json!({"caller_server":foreign})
        } else {
            json!({"caller_server":foreign,"pane":"%7","role":"coordinator"})
        };
        client.send(json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"foreign","method":method,"params":params})).await;
        assert_eq!(client.recv().await["error"]["code"], "refused");
        assert_eq!(mock.call_count(), 1);
        client.require_next_pong().await;
    }
    server.stop().await;
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test]
async fn message_bounds_precede_disabled_service_on_real_socket() {
    let server = Server::start().await;
    let mut client = server.client().await;
    client.connect().await;
    for (method, params) in [
        ("msg.send", json!({"to":"owner","body":"x".repeat(16385)})),
        (
            "msg.send",
            json!({"to":"owner","body":"x","attachments":vec!["p";33]}),
        ),
        (
            "msg.send",
            json!({"to":"owner","body":"x","attachments":[""]}),
        ),
        (
            "msg.send",
            json!({"to":"owner","body":"x","attachments":["x".repeat(4097)]}),
        ),
        (
            "msg.send",
            json!({"to":"owner","body":"x","task":"x".repeat(257)}),
        ),
        ("msg.inbox", json!({"limit":0})),
        ("msg.inbox", json!({"limit":1001})),
        ("msg.inbox", json!({"id":"m1","after":"m2"})),
        ("msg.inbox", json!({"count_only":true,"id":"m1"})),
        ("msg.inbox", json!({"count_only":true,"after":"m1"})),
        ("msg.ack", json!({"ids":[]})),
        (
            "msg.ack",
            json!({"ids":(1..=1001).map(|n|format!("m{n}")).collect::<Vec<_>>()}),
        ),
    ] {
        client
            .send(
                serde_json::to_value(ClientMessage::request(
                    "bounds".into(),
                    method.into(),
                    params,
                    None,
                ))
                .unwrap(),
            )
            .await;
        assert_eq!(
            client.recv().await["error"]["code"],
            "invalid_params",
            "{method}"
        );
        client.require_next_pong().await;
    }
    client
        .send(
            serde_json::to_value(ClientMessage::request(
                "boundary".into(),
                "msg.send".into(),
                json!({"to":"owner","body":"x".repeat(16384)}),
                None,
            ))
            .unwrap(),
        )
        .await;
    assert_eq!(client.recv().await["error"]["code"], "unavailable");
    client.require_next_pong().await;
    server.stop().await;
}

#[tokio::test]
async fn watch_refused_unwatch_noop_and_legacy_traffic_unchanged() {
    let server = Server::start().await;
    let mut client = server.client().await;
    let connected = client.connect().await;
    assert_eq!(connected["type"], "connected");
    // A trailing ping makes an ignored watch observable without waiting for an idle timeout.
    client
        .send(serde_json::to_value(ClientMessage::watch(vec![], 0)).unwrap())
        .await;
    client
        .send(serde_json::to_value(ClientMessage::ping(7)).unwrap())
        .await;
    let watch = client.recv().await;
    server.stop().await;
    assert_eq!(watch["type"], "error");
    assert_eq!(watch["code"], "unavailable");
    assert!(watch["message"]
        .as_str()
        .unwrap()
        .contains("not configured"));
    assert!(watch.get("id").is_none());
}

#[tokio::test]
async fn info_methods_match_handshake_and_registered_names_with_default_and_injected_services() {
    let dir = tempfile::tempdir().unwrap();
    for injected in [false, true] {
        let mut services = MethodServices::new(spawn_registry());
        let expected_dir = if injected {
            services.state_dir = dir.path().to_owned();
            dir.path().to_owned()
        } else {
            atmd::eventlog::state_dir()
        };
        let names = router(services.clone()).unwrap().names();
        assert!(names.iter().any(|name| name == "daemon.info"));
        let server = Server::with_services(services).await;
        let mut client = server.client().await;
        let connected = client.connect().await;
        client
            .send(
                serde_json::to_value(ClientMessage::request(
                    "info".into(),
                    "daemon.info".into(),
                    json!({}),
                    None,
                ))
                .unwrap(),
            )
            .await;
        let response = client.recv().await;
        assert_eq!(response["result"]["methods"], connected["methods"]);
        assert_eq!(connected["methods"], json!(names));
        assert_eq!(response["result"]["protocol"], "1.1");
        assert_eq!(response["result"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(response["result"]["state_dir"], json!(expected_dir));
        client.require_next_pong().await;
        server.stop().await;
    }
}

#[tokio::test]
async fn agent_methods_are_advertised_enabled_or_explicitly_disabled() {
    use atmd::org::agents::AgentHandle;
    for injected in [false, true] {
        let state = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let missing_root = state.path().join("missing");
        let mut services = MethodServices::new(spawn_registry());
        services.org = atmd::org::OrgHandle::open(state.path().into(), cancel.clone()).await;
        if injected {
            services.agents = Some(AgentHandle::disabled("maintenance"));
        }
        let server = Server::with_services(services).await;
        let mut client = server.client().await;
        let connected = client.connect().await;
        client.send(json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"info","method":"daemon.info","params":{}})).await;
        let info = client.recv().await;
        assert_eq!(info["result"]["methods"], connected["methods"]);
        client.require_next_pong().await;
        if !injected {
            // Synchronize with initial reconciliation before testing mutations.
            client.send(json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"initial-list","method":"agent.list","params":{}})).await;
            assert_eq!(
                client.recv().await,
                json!({"type":"response","id":"initial-list","result":[]})
            );
            client.require_next_pong().await;
        }
        for (method, params) in [
            (
                "agent.up",
                json!({"project_root":missing_root,"name":"lead"}),
            ),
            (
                "agent.adopt",
                json!({"project_root":missing_root,"name":"lead","pane":"%7"}),
            ),
            ("agent.clear", json!({"agent":"project/lead"})),
            ("agent.respawn", json!({"agent":"project/lead"})),
            ("agent.down", json!({"agent":"project/lead"})),
            ("agent.list", Value::Null),
        ] {
            assert!(connected["methods"]
                .as_array()
                .unwrap()
                .contains(&json!(method)));
            client.send(json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":method,"method":method,"params":params})).await;
            let expected = if injected {
                json!({"type":"response","id":method,"error":{"code":"unavailable","message":"agent service unavailable: maintenance"}})
            } else if method == "agent.list" {
                json!({"type":"response","id":method,"result":[]})
            } else if matches!(method, "agent.up" | "agent.adopt") {
                let message = format!("{}: directory cannot be resolved", missing_root.display());
                json!({"type":"response","id":method,"error":{"code":"refused","message":message}})
            } else {
                json!({"type":"response","id":method,"error":{"code":"not_found","message":"agent project/lead was not found"}})
            };
            assert_eq!(client.recv().await, expected);
            client.require_next_pong().await;
        }
        // Semantic errors win even over an explicitly injected disabled reason.
        client.send(json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"invalid","method":"agent.clear","params":{"agent":"project/lead","timeout_ms":0}})).await;
        assert_eq!(
            client.recv().await,
            json!({"type":"response","id":"invalid","error":{"code":"invalid_params","message":"invalid agent parameters: timeout_ms must be in 5000..=120000"}})
        );
        client.require_next_pong().await;
        server.stop().await;
        cancel.cancel();
    }
}

#[tokio::test]
#[should_panic(expected = "fixture emitted an extra reply")]
async fn fixture_barrier_rejects_an_extra_response() {
    let (ours, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(b"{\"type\":\"response\",\"id\":\"extra\",\"result\":null}\n")
        .await
        .unwrap();
    let (read, write) = ours.into_split();
    let mut client = Client {
        read: BufReader::new(read),
        write,
    };
    client.require_next_pong().await;
}

#[tokio::test(flavor = "current_thread")]
async fn server_shutdown_does_not_wait_for_stuck_subscriber_drain_limit() {
    let logs = tempfile::tempdir().unwrap();
    let log = logs.path().join("shutdown.log");
    // Only this runtime thread's servers belong in the trace. A second live
    // dispatcher avoids tracing-core's single-dispatch callsite-interest race
    // when parallel tests first hit a callsite with no subscriber installed.
    let _fallback = tracing::subscriber::set_default(tracing::subscriber::NoSubscriber::default());
    let _trace = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(std::sync::Mutex::new(std::fs::File::create(&log).unwrap()))
            .finish(),
    );
    let registry = spawn_registry();
    let mut server = Server::with_services(MethodServices::new(registry.clone())).await;
    let mut client = server.client().await;
    client.connect().await;
    client
        .send(serde_json::to_value(ClientMessage::subscribe(None)).unwrap())
        .await;
    assert_eq!(client.recv().await["type"], "session_list");
    let id = atm_core::SessionId::new("stuck-reader");
    let mut session = atm_core::SessionDomain::new(
        id.clone(),
        atm_core::AgentType::GeneralPurpose,
        atm_core::Model::Unknown,
    );
    // Working directories are shortened in SessionView; they cannot stall a
    // writer reliably. First prompts retain their full contents on the wire.
    session.first_prompt = Some("x".repeat(4 * 1024 * 1024));
    registry.register(session).await.unwrap();
    registry
        .update_from_status_line(id.clone(), None, Default::default(), atm_core::Harness::Pi)
        .await
        .unwrap();
    assert!(
        serde_json::to_vec(&registry.get_session(id).await.unwrap())
            .unwrap()
            .len()
            > 4 * 1024 * 1024
    );
    // At least one byte arrived: the forwarder is writing the oversized view.
    timeout(Duration::from_secs(60), client.read.read_u8())
        .await
        .unwrap()
        .unwrap();
    server.cancel.cancel();
    let stopped = timeout(Duration::from_secs(60), &mut server.task).await;
    if stopped.is_err() {
        server.task.abort();
    }
    assert!(stopped.is_ok(), "shutdown hit the connection drain limit");
    stopped.unwrap().unwrap().unwrap();
    // Completion alone could hide the fallback abort after the drain deadline.
    // Observe that branch directly instead of asserting elapsed wall time.
    let trace = std::fs::read_to_string(log).unwrap();
    assert_eq!(
        trace.matches("Server cleanup complete").count(),
        1,
        "{trace}"
    );
    assert!(!trace.contains("Aborting connection handlers"), "{trace}");
}

#[tokio::test]
async fn legacy_session_client_needs_no_requests() {
    let server = Server::start().await;
    let mut client = server.client().await;
    let connected = client.connect().await;
    assert_eq!(connected["client_id"], "conformance");
    for message in [
        ClientMessage::list_sessions(),
        ClientMessage::subscribe(None),
    ] {
        client.send(serde_json::to_value(message).unwrap()).await;
        assert_eq!(
            client.recv().await,
            json!({"type":"session_list","sessions":[]})
        );
    }
    for message in [
        ClientMessage::new(atm_protocol::MessageType::Unsubscribe),
        ClientMessage::unwatch(),
        ClientMessage::ping(8),
    ] {
        client.send(serde_json::to_value(message).unwrap()).await;
    }
    assert!(matches!(
        serde_json::from_value::<DaemonMessage>(client.recv().await).unwrap(),
        DaemonMessage::Pong { seq: 8 }
    ));
    client
        .send(serde_json::to_value(ClientMessage::disconnect()).unwrap())
        .await;
    let mut line = String::new();
    assert_eq!(
        timeout(Duration::from_secs(5), client.read.read_line(&mut line))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    server.stop().await;
}
