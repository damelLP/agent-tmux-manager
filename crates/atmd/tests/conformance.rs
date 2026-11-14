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
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

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
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let cancel = CancellationToken::new();
        let server = DaemonServer::with_services(&socket, services, cancel.clone());
        let task = tokio::spawn(async move { server.run().await });
        timeout(Duration::from_secs(5), async {
            while !socket.exists() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
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

#[tokio::test]
async fn replay_all_protocol_fixtures() {
    let mut paths = Vec::new();
    files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/protocol"),
        &mut paths,
    );
    paths.sort();
    assert!(!paths.is_empty());
    for path in paths {
        let fixture: Fixture = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let server = Server::start().await;
        let mut client = server.client().await;
        client.connect().await;
        client.send(fixture.request).await;
        let actual = client.recv().await;
        let compared = compare(actual, fixture.response, &fixture.ignore);
        client.require_next_pong().await;
        server.stop().await;
        assert!(compared.is_ok(), "{}: {compared:?}", path.display());
    }
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
    assert_eq!(
        watch,
        json!({"type":"error","code":"unavailable","message":"event watching is not available yet"})
    );
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

#[tokio::test]
async fn server_shutdown_does_not_wait_for_stuck_subscriber_drain_limit() {
    let registry = spawn_registry();
    let mut server = Server::with_services(MethodServices::new(registry.clone())).await;
    let mut client = server.client().await;
    client.connect().await;
    client
        .send(serde_json::to_value(ClientMessage::subscribe(None)).unwrap())
        .await;
    assert_eq!(client.recv().await["type"], "session_list");
    registry
        .register_discovered(
            atm_core::SessionId::new("stuck-reader"),
            1234,
            PathBuf::from("x".repeat(4 * 1024 * 1024)),
            None,
            atm_core::Harness::Pi,
        )
        .await
        .unwrap();
    // At least one byte arrived: the forwarder is writing the oversized view.
    timeout(Duration::from_secs(5), client.read.read_u8())
        .await
        .unwrap()
        .unwrap();
    server.cancel.cancel();
    let stopped = timeout(Duration::from_millis(500), &mut server.task).await;
    if stopped.is_err() {
        server.task.abort();
    }
    assert!(stopped.is_ok(), "shutdown hit the connection drain limit");
    stopped.unwrap().unwrap().unwrap();
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
