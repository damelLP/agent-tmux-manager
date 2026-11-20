//! Integration tests for the Unix socket server.
//!
//! These tests verify the DaemonServer works correctly as a complete system,
//! testing connection handling, protocol negotiation, subscriptions, and graceful shutdown.
//!
//! Per CLAUDE.md: Tests CAN use `.unwrap()` and `.expect()` - this is allowed.
//! We test the panic-free behavior of production code through assertions.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::path::PathBuf;
use std::time::Duration;

use atm_core::{AgentType, Harness, LifecycleEvent, Model, SessionDomain, SessionId, SessionView};
use atm_protocol::{ClientMessage, DaemonMessage, MessageType, ProtocolVersion};
use atmd::registry::spawn_registry;
use atmd::server::{DaemonServer, MAX_TUI_CLIENTS};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

// ============================================================================
// Constants
// ============================================================================

/// Outer hang guard for bind and owned-task completion, never a settle delay.
const SOCKET_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

// ============================================================================
// Test Helpers
// ============================================================================

/// Test server context that manages server lifecycle and cleanup.
struct TestServer {
    socket_path: PathBuf,
    cancel_token: CancellationToken,
    run_task: tokio::task::JoinHandle<()>,
    _temp_dir: TempDir, // Keep alive for RAII cleanup
}

impl TestServer {
    /// Internal helper that spawns the server and returns both the test server and registry handle.
    async fn spawn_internal() -> (Self, atmd::registry::RegistryHandle) {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let registry = spawn_registry();
        let services = atmd::server::methods::MethodServices::new(registry.clone());
        let server = Self::spawn_with_services(temp_dir, CancellationToken::new(), services).await;
        (server, registry)
    }

    async fn spawn_with_services(
        temp_dir: TempDir,
        cancel_token: CancellationToken,
        services: atmd::server::methods::MethodServices,
    ) -> Self {
        let socket_path = temp_dir.path().join("test.sock");
        let server =
            DaemonServer::with_services(socket_path.clone(), services, cancel_token.clone());

        let (ready, started) = tokio::sync::oneshot::channel();
        let run_task = tokio::spawn(async move {
            let run = server.run();
            tokio::pin!(run);
            // The default server binds synchronously before its first Pending
            // accept. Keep that exact future alive after registering readiness.
            tokio::select! {
                biased;
                result = &mut run => panic!("server exited before binding: {result:?}"),
                _ = std::future::ready(()) => { let _ = ready.send(()); }
            }
            run.await.expect("server run");
        });
        timeout(SOCKET_WAIT_TIMEOUT, started)
            .await
            .unwrap()
            .unwrap();

        // Fail fast if socket didn't appear
        assert!(
            socket_path.exists(),
            "Server socket did not appear within {SOCKET_WAIT_TIMEOUT:?}"
        );

        Self {
            socket_path,
            cancel_token,
            run_task,
            _temp_dir: temp_dir,
        }
    }

    /// Spawns a new test server in the background.
    async fn spawn() -> Self {
        Self::spawn_internal().await.0
    }

    /// Spawns a test server with access to the registry handle.
    async fn spawn_with_registry() -> (Self, atmd::registry::RegistryHandle) {
        Self::spawn_internal().await
    }

    /// Creates a client connection to the server.
    async fn connect(&self) -> TestClient {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .expect("connect to server");
        TestClient::new(stream)
    }

    /// Shuts down the server gracefully.
    async fn shutdown(self) {
        self.cancel_token.cancel();
        timeout(Duration::from_secs(60), self.run_task)
            .await
            .unwrap()
            .unwrap();
    }
}

/// Test client connection with protocol helpers.
struct TestClient<R = tokio::net::unix::OwnedReadHalf, W = tokio::net::unix::OwnedWriteHalf> {
    reader: BufReader<R>,
    writer: W,
}

impl TestClient {
    fn new(stream: UnixStream) -> Self {
        let (reader, writer) = stream.into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> TestClient<R, W> {
    async fn applied(&mut self, seq: u64) {
        timeout(Duration::from_secs(60), async {
            self.send(ClientMessage::ping(seq)).await;
            match self.recv().await {
                DaemonMessage::Pong { seq: got } if got == seq => {}
                other => panic!("expected Pong {seq}, got {other:?}"),
            }
        })
        .await
        .expect("same-stream apply fence");
    }

    /// Sends a message to the server.
    async fn send(&mut self, msg: ClientMessage) {
        let json = serde_json::to_string(&msg).unwrap();
        self.writer.write_all(json.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
        self.writer.flush().await.unwrap();
    }

    /// Receives a message from the server.
    async fn recv(&mut self) -> DaemonMessage {
        let mut line = String::new();
        self.reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// Performs handshake with optional client ID.
    async fn handshake(&mut self, client_id: Option<String>) -> String {
        self.send(ClientMessage::connect(client_id)).await;

        match self.recv().await {
            DaemonMessage::Connected { client_id, .. } => client_id,
            other => panic!("Expected Connected, got {other:?}"),
        }
    }

    /// Performs handshake with a specific protocol version.
    async fn handshake_with_version(&mut self, version: ProtocolVersion) -> DaemonMessage {
        let msg = ClientMessage {
            protocol_version: version,
            message: MessageType::Connect { client_id: None },
        };
        self.send(msg).await;
        self.recv().await
    }
}

mod plan_usage_contract {
    use super::*;
    use atm_org::OrgCommand;
    use atm_protocol::{error_code, ResponsePayload};
    use atmd::{org::OrgHandle, registry::RegistryHandle, server::methods::MethodServices};
    use serde_json::{json, Value};

    async fn rig() -> (TestServer, RegistryHandle) {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("org"), cancel.child_token()).await;
        let now = chrono::Utc::now();
        org.execute(
            OrgCommand::RegisterProject {
                project_id: "fixture".parse().unwrap(),
                root: dir.path().to_str().unwrap().into(),
                move_root: false,
            },
            now,
        )
        .await
        .unwrap();
        let registry = spawn_registry();
        for (agent, pane, bound) in [
            ("alpha", "%1", true),
            ("bravo", "%2", true),
            ("unbound", "%3", false),
        ] {
            let mut row = create_test_session(agent);
            row.harness = Harness::ClaudeCode;
            row.tmux_pane = Some(pane.into());
            org.execute(
                OrgCommand::Adopt {
                    agent_id: format!("fixture/{agent}").parse().unwrap(),
                    pane: pane.into(),
                    session_id: bound.then(|| row.id.clone()),
                },
                now,
            )
            .await
            .unwrap();
            registry.register(row).await.unwrap();
        }
        let mut services = MethodServices::new(registry.clone());
        services.org = org;
        services.state_dir = dir.path().join("state");
        // The exact committed bindings are the fixture; don't run tmux reconciliation.
        services.agents = Some(atmd::org::agents::AgentHandle::disabled("fixture"));
        (
            TestServer::spawn_with_services(dir, cancel, services).await,
            registry,
        )
    }

    async fn usage(client: &mut TestClient, pane: Option<&str>, params: Value) -> ResponsePayload {
        client
            .send(ClientMessage::request(
                "usage".into(),
                "usage.list".into(),
                params,
                pane.map(str::to_owned),
            ))
            .await;
        match timeout(SOCKET_WAIT_TIMEOUT, client.recv()).await.unwrap() {
            DaemonMessage::Response { id, payload } if id == "usage" => payload,
            other => panic!("expected usage response, got {other:?}"),
        }
    }

    async fn owner_usage(client: &mut TestClient) -> Value {
        match usage(client, None, json!({})).await {
            ResponsePayload::Result(value) => value,
            other => panic!("owner usage query failed: {other:?}"),
        }
    }

    fn status(session: &str, pane: &str, quota: Value, tokens: u32) -> ClientMessage {
        ClientMessage::status_update(json!({
            "session_id": session, "tmux_pane": pane, "rate_limits": quota,
            "context_window": {"context_window_size":100000,"current_usage":{"input_tokens": tokens}}
        }))
    }

    #[tokio::test]
    async fn usage_list_capability_and_scope() {
        let (server, _) = rig().await;
        let mut client = server.connect().await;
        client.send(ClientMessage::connect(None)).await;
        match timeout(SOCKET_WAIT_TIMEOUT, client.recv()).await.unwrap() {
            DaemonMessage::Connected { methods, .. } => assert!(
                methods.iter().any(|m| m == "usage.list"),
                "usage.list is not advertised"
            ),
            other => panic!("expected Connected, got {other:?}"),
        }
        assert_eq!(owner_usage(&mut client).await, json!([]));
        send_applied(
            &mut client,
            1,
            status(
                "alpha",
                "%1",
                json!({"five_hour":{"used_percentage":12}}),
                1000,
            ),
        )
        .await;
        assert_eq!(
            owner_usage(&mut client).await[0]["windows"][0]["used_percent"],
            json!(12.0)
        );
        for (pane, params, code) in [
            (Some("%1"), json!({}), error_code::REFUSED),
            (Some("%2"), json!({}), error_code::REFUSED),
            (Some("invalid"), json!({}), error_code::INVALID_PARAMS),
            (
                None,
                json!({"profile":"claude"}),
                error_code::INVALID_PARAMS,
            ),
        ] {
            match usage(&mut client, pane, params).await {
                ResponsePayload::Error(error) => assert_eq!(error.code, code),
                other => panic!("expected refusal, got {other:?}"),
            }
        }
        assert_eq!(owner_usage(&mut client).await.as_array().unwrap().len(), 1);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn status_updates_from_two_bound_sources_share_one_sample_without_changing_context() {
        // Captured: Claude Code 2.1.329, 2026-11-19, ATM status-line push (quota only).
        // Synthetic: the local session, pane and context around the real quota fields.
        let Some(lines) = captured_hook_payloads(
            "atm-claude-adapter",
            "claude_status_line_quota_2.1.329.jsonl",
        ) else {
            return;
        };
        let captured = lines.first().expect("captured quota line");
        let (server, registry) = rig().await;
        let mut client = server.connect().await;
        client.handshake(None).await;
        send_applied(
            &mut client,
            1,
            status("alpha", "%1", captured["rate_limits"].clone(), 1000),
        )
        .await;
        let first = owner_usage(&mut client).await;
        assert_eq!(
            first[0]["windows"],
            json!([{"scope":null,"duration_minutes":10080,"used_percent":52.0,"resets_at":"2026-11-22T21:00:00Z"}])
        );

        send_applied(
            &mut client,
            2,
            status(
                "alpha",
                "%1",
                json!({"seven_day":{"used_percentage":"malformed"}}),
                7000,
            ),
        )
        .await;
        let row = registry.get_session(SessionId::new("alpha")).await.unwrap();
        assert!((row.context_percentage - 7.0).abs() < 1e-9);
        assert_eq!(
            owner_usage(&mut client).await[0]["windows"],
            first[0]["windows"],
            "malformed quota erased the sample"
        );

        send_applied(
            &mut client,
            3,
            status(
                "unbound",
                "%3",
                json!({"five_hour":{"used_percentage":95}}),
                1000,
            ),
        )
        .await;
        send_applied(
            &mut client,
            4,
            status(
                "alpha",
                "%2",
                json!({"five_hour":{"used_percentage":95}}),
                1000,
            ),
        )
        .await;
        assert_eq!(
            owner_usage(&mut client).await[0]["windows"],
            first[0]["windows"],
            "unproven source overwrote the sample"
        );

        send_applied(
            &mut client,
            5,
            status(
                "bravo",
                "%2",
                json!({"five_hour":{"used_percentage":0}}),
                1000,
            ),
        )
        .await;
        let next = owner_usage(&mut client).await;
        assert_eq!(next.as_array().unwrap().len(), 1);
        assert_eq!(next[0]["profile"], "claude");
        assert_eq!(next[0]["meter"], "claude");
        assert_eq!(next[0]["source"], "claude_code");
        assert_eq!(
            next[0]["windows"],
            json!([{"scope":null,"duration_minutes":300,"used_percent":0.0,"resets_at":null}])
        );
        server.shutdown().await;
    }
}

type MemoryClient = TestClient<
    tokio::io::ReadHalf<tokio::io::DuplexStream>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
>;

struct HandlerServer {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl HandlerServer {
    fn new(registry: atmd::registry::RegistryHandle) -> (Self, MemoryClient) {
        let (stream, peer) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(stream);
        let methods = atmd::server::methods::router(atmd::server::methods::MethodServices::new(
            registry.clone(),
        ))
        .unwrap();
        let cancel = CancellationToken::new();
        let handler = atmd::server::ConnectionHandler::new(
            reader,
            writer,
            registry,
            Default::default(),
            1,
            cancel.clone(),
            std::sync::Arc::new(methods),
        );
        let task = tokio::spawn(async move {
            handler.run().await;
        });
        let (reader, writer) = tokio::io::split(peer);
        (
            Self { cancel, task },
            TestClient {
                reader: BufReader::new(reader),
                writer,
            },
        )
    }

    async fn shutdown(self) {
        self.cancel.cancel();
        timeout(Duration::from_secs(60), self.task)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn same_stream_fence_waits_for_a_held_registry_apply() {
    use atmd::registry::RegistryCommand;
    use std::future::{poll_fn, Future};
    use std::task::Poll;

    let (commands, mut received) = tokio::sync::mpsc::channel(16);
    let (events, _) = tokio::sync::broadcast::channel(16);
    let registry = atmd::registry::RegistryHandle::new(commands, events);
    let (server, mut client) = HandlerServer::new(registry);
    client.handshake(None).await;
    {
        let mut applied = std::pin::pin!(send_applied(
            &mut client,
            7,
            ClientMessage::hook_event(hook_event_json(
                "held",
                "PreToolUse",
                serde_json::json!({"tool_name":"Bash"})
            ))
        ));
        let command = timeout(Duration::from_secs(60), async {
            tokio::select! {
                biased;
                _ = &mut applied => panic!("frame send returned before registry apply"),
                command = received.recv() => command.unwrap(),
            }
        })
        .await
        .unwrap();
        let RegistryCommand::ApplyLifecycleEvent { respond_to, .. } = command else {
            panic!("expected lifecycle apply")
        };
        assert!(poll_fn(|cx| Poll::Ready(applied.as_mut().poll(cx).is_pending())).await);
        respond_to.send(Ok(())).unwrap();
        timeout(Duration::from_secs(60), applied).await.unwrap();
    }
    server.shutdown().await;
}

async fn drain_failed_writer<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    failure: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
) -> String {
    timeout(Duration::from_secs(60), failure.recv())
        .await
        .expect("writer did not fail")
        .expect("writer failure observer closed");
    let mut output = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            break;
        }
        output.push_str(&line);
    }
    output
}

#[tokio::test]
async fn failed_writer_fence_retains_partial_output_and_blocks_early_drain() {
    use std::future::{poll_fn, Future};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::task::{Context, Poll};
    struct Probe {
        read: tokio::io::DuplexStream,
        calls: Arc<AtomicUsize>,
    }
    impl AsyncRead for Probe {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::pin::Pin::new(&mut self.read).poll_read(cx, bytes)
        }
    }
    let (read, mut write) = tokio::io::duplex(64);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut reader = BufReader::new(Probe {
        read,
        calls: calls.clone(),
    });
    let (failed, mut failure) = tokio::sync::mpsc::unbounded_channel();
    write.write_all(b"partial frame").await.unwrap();
    let mut drain = std::pin::pin!(drain_failed_writer(&mut reader, &mut failure));
    assert!(poll_fn(|cx| Poll::Ready(drain.as_mut().poll(cx).is_pending())).await);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "reader relieved backpressure before writer failed"
    );
    failed.send(()).unwrap();
    drop(write);
    assert_eq!(drain.await, "partial frame");

    // A failure delivered before the waiter must be retained too.
    let (mut write, read) = tokio::io::duplex(64);
    write.write_all(b"already failed\n").await.unwrap();
    drop(write);
    let (failed, mut failure) = tokio::sync::mpsc::unbounded_channel();
    failed.send(()).unwrap();
    assert_eq!(
        drain_failed_writer(&mut BufReader::new(read), &mut failure).await,
        "already failed\n"
    );
}

/// Observe the existing failed-write event on this current-thread test runtime.
/// Registration precedes server spawn, so an early failure remains queued.
fn failed_writer_events() -> (
    tracing::dispatcher::DefaultGuard,
    tokio::sync::mpsc::UnboundedReceiver<()>,
) {
    use tracing_subscriber::prelude::*;
    #[derive(Default)]
    struct Fields {
        message: String,
        error: String,
    }
    impl tracing::field::Visit for Fields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            match field.name() {
                "message" => self.message = format!("{value:?}"),
                "error" => self.error = format!("{value:?}"),
                _ => {}
            }
        }
    }
    struct Failed(tokio::sync::mpsc::UnboundedSender<()>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Failed {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            // Both ordinary and error replies propagate failed writes to run().
            // The error-reply branch does not log a second handler error.
            if fields.message == "Connection closed" && fields.error == "Write timeout" {
                let _ = self.0.send(());
            }
        }
    }
    let (send, receive) = tokio::sync::mpsc::unbounded_channel();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(Failed(send)));
    (tracing::dispatcher::set_default(&dispatch), receive)
}

/// Helper to create a test session.
fn create_test_session(id: &str) -> SessionDomain {
    SessionDomain::new(
        SessionId::new(id),
        AgentType::GeneralPurpose,
        Model::Sonnet4,
    )
}

// ============================================================================
// Connection Tests
// ============================================================================

#[tokio::test]
async fn test_server_accepts_connection() {
    let server = TestServer::spawn().await;

    // Should be able to connect
    let _client = server.connect().await;

    server.shutdown().await;
}

#[tokio::test]
async fn test_handshake_success() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    // Send connect with client ID
    client
        .send(ClientMessage::connect(Some("test-client".to_string())))
        .await;

    // Should receive Connected
    match client.recv().await {
        DaemonMessage::Connected {
            protocol_version,
            client_id,
            ..
        } => {
            assert_eq!(protocol_version, ProtocolVersion::CURRENT);
            assert_eq!(client_id, "test-client");
        }
        other => panic!("Expected Connected, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn test_legacy_hook_connect_is_accepted() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;
    let response = timeout(Duration::from_secs(2), async {
        // Literal atm-hook handshake with a fixed session ID and PID.
        client.writer.write_all(concat!(
            r#"{"protocol_version":{"major":1,"minor":0},"type":"connect","client_id":"hook-source-check-1063205"}"#,
            "\n"
        ).as_bytes()).await.unwrap();
        client.recv().await
    }).await.expect("legacy handshake before timeout");
    assert!(
        matches!(response, DaemonMessage::Connected { client_id, .. }
        if client_id == "hook-source-check-1063205")
    );
    server.shutdown().await;
}

#[tokio::test]
async fn test_request_placeholder_returns_unknown_method_with_id() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;
    client.handshake(None).await;
    timeout(Duration::from_secs(2), async {
        client.writer.write_all(concat!(
            r#"{"protocol_version":{"major":1,"minor":1},"type":"request","id":"r1","method":"x.y"}"#,
            "\n"
        ).as_bytes()).await.unwrap();
        let mut line = String::new();
        client.reader.read_line(&mut line).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response, serde_json::json!({
            "type": "response",
            "id": "r1",
            "error": {"code": "unknown_method", "message": "no method 'x.y'"}
        }));
    }).await.expect("request response before timeout");
    server.shutdown().await;
}

#[tokio::test]
async fn test_handshake_auto_assigns_client_id() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    // Send connect without client_id
    client.send(ClientMessage::connect(None)).await;

    // Should receive Connected with auto-assigned ID
    match client.recv().await {
        DaemonMessage::Connected { client_id, .. } => {
            assert!(
                client_id.starts_with("client-"),
                "Expected auto-assigned ID starting with 'client-', got: {client_id}"
            );
        }
        other => panic!("Expected Connected, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn test_handshake_version_mismatch() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    // Send connect with incompatible version (major version 99)
    let response = client
        .handshake_with_version(ProtocolVersion::new(99, 0))
        .await;

    // Should receive Rejected
    match response {
        DaemonMessage::Rejected { reason, .. } => {
            assert!(
                reason.contains("not compatible"),
                "Expected 'not compatible' in reason, got: {reason}"
            );
        }
        other => panic!("Expected Rejected, got {other:?}"),
    }

    server.shutdown().await;
}

// ============================================================================
// Subscribe/Unsubscribe Flow Tests
// ============================================================================

#[tokio::test]
async fn test_subscribe_unsubscribe_flow() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    // Handshake
    client.handshake(Some("sub-client".to_string())).await;

    // Subscribe (no filter)
    client.send(ClientMessage::subscribe(None)).await;

    // Should receive session list (initially empty)
    match client.recv().await {
        DaemonMessage::SessionList { sessions } => {
            assert_eq!(sessions.len(), 0, "Initial session list should be empty");
        }
        other => panic!("Expected SessionList, got {other:?}"),
    }

    // Unsubscribe
    client
        .send(ClientMessage::new(MessageType::Unsubscribe))
        .await;

    // Can still send other messages after unsubscribe
    client.send(ClientMessage::list_sessions()).await;
    match client.recv().await {
        DaemonMessage::SessionList { .. } => {}
        other => panic!("Expected SessionList after unsubscribe, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn test_subscribe_with_session_filter() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    // Subscribe with session filter
    let session_id = SessionId::new("specific-session");
    client
        .send(ClientMessage::subscribe(Some(session_id)))
        .await;

    // Should receive session list
    match client.recv().await {
        DaemonMessage::SessionList { .. } => {}
        other => panic!("Expected SessionList, got {other:?}"),
    }

    server.shutdown().await;
}

// ============================================================================
// Broadcast Tests
// ============================================================================

#[tokio::test]
async fn test_broadcast_respects_filter() {
    // NOTE: The current implementation has an incomplete subscription integration.
    // ConnectionHandler tracks subscription state internally but doesn't register
    // clients with DaemonServer.subscribers for event broadcasting.
    //
    // This test verifies the infrastructure works correctly:
    // 1. Clients can subscribe with or without filters
    // 2. They receive the initial session list
    // 3. Sessions can be registered and updated via registry
    //
    // When broadcast integration is completed, this test should be updated to
    // verify that events are actually delivered to subscribed clients.

    let (server, registry) = TestServer::spawn_with_registry().await;

    // Client 1: subscribes to session "target"
    let mut client1 = server.connect().await;
    client1.handshake(Some("client-1".to_string())).await;
    client1
        .send(ClientMessage::subscribe(Some(SessionId::new("target"))))
        .await;

    // Should receive initial session list (empty)
    match client1.recv().await {
        DaemonMessage::SessionList { sessions } => {
            assert_eq!(sessions.len(), 0, "Initial session list should be empty");
        }
        other => panic!("Expected SessionList, got {other:?}"),
    }

    // Client 2: subscribes to all sessions
    let mut client2 = server.connect().await;
    client2.handshake(Some("client-2".to_string())).await;
    client2.send(ClientMessage::subscribe(None)).await;

    // Should receive initial session list (empty)
    match client2.recv().await {
        DaemonMessage::SessionList { sessions } => {
            assert_eq!(sessions.len(), 0, "Initial session list should be empty");
        }
        other => panic!("Expected SessionList, got {other:?}"),
    }

    // Register sessions via registry
    let target_session = create_test_session("target");
    registry
        .register(target_session)
        .await
        .expect("register target session");

    let other_session = create_test_session("other");
    registry
        .register(other_session)
        .await
        .expect("register other session");

    // Verify sessions were registered by querying via list_sessions
    client1.send(ClientMessage::list_sessions()).await;
    match client1.recv().await {
        DaemonMessage::SessionList { sessions } => {
            assert_eq!(sessions.len(), 2, "Should have 2 sessions registered");
        }
        other => panic!("Expected SessionList with 2 sessions, got {other:?}"),
    }

    server.shutdown().await;
}

// ============================================================================
// Max Clients Tests
// ============================================================================

#[tokio::test]
async fn test_max_clients_rejection() {
    let server = TestServer::spawn().await;
    let mut clients = Vec::new();

    // Connect and subscribe MAX_TUI_CLIENTS clients
    for i in 0..MAX_TUI_CLIENTS {
        let mut client = server.connect().await;
        client.handshake(Some(format!("client-{i}"))).await;
        client.send(ClientMessage::subscribe(None)).await;
        let _ = client.recv().await; // drain session list
        clients.push(client);
    }

    // Verify all MAX_TUI_CLIENTS clients are working by sending ping
    for (i, client) in clients.iter_mut().enumerate() {
        client.send(ClientMessage::ping(i as u64)).await;
        match client.recv().await {
            DaemonMessage::Pong { seq } => {
                assert_eq!(seq, i as u64);
            }
            other => panic!("Expected Pong for client {i}, got {other:?}"),
        }
    }

    // One more client can connect and handshake, but the daemon answers
    // its Subscribe with an Error naming the cap.
    let mut extra = server.connect().await;
    extra.handshake(Some("client-extra".to_string())).await;
    extra.send(ClientMessage::subscribe(None)).await;
    match extra.recv().await {
        DaemonMessage::Error { message, .. } => {
            let expected = format!("Too many subscribers (max: {MAX_TUI_CLIENTS})");
            assert_eq!(message, expected);
        }
        other => panic!("Expected a Too many subscribers error, got {other:?}"),
    }

    server.shutdown().await;
}

/// The cap counts subscribed connections, not client ids: clients choose
/// their ids, and every subscribed connection runs its own forwarder.
#[tokio::test]
async fn test_max_clients_rejects_connection_reusing_a_client_id() {
    let server = TestServer::spawn().await;
    let mut clients = Vec::new();
    for i in 0..MAX_TUI_CLIENTS {
        clients.push(subscribe_client(&server, &format!("client-{i}")).await);
    }

    let mut reused = server.connect().await;
    reused.handshake(Some("client-0".to_string())).await;
    reused.send(ClientMessage::subscribe(None)).await;

    match reused.recv().await {
        DaemonMessage::Error { message, .. } => {
            assert!(message.contains("Too many subscribers"), "got {message:?}");
        }
        other => panic!("Expected a Too many subscribers error, got {other:?}"),
    }

    server.shutdown().await;
}

// ============================================================================
// Graceful Shutdown Tests
// ============================================================================

#[tokio::test]
async fn test_graceful_shutdown() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    let socket_path = server.socket_path.clone();

    // Trigger shutdown
    server.cancel_token.cancel();

    // Join the owned server before observing its cleanup.
    server.shutdown().await;

    // Socket should be removed
    assert!(
        !socket_path.exists(),
        "Socket file should be removed after shutdown"
    );
}

// ============================================================================
// Slow Subscriber Tests
// ============================================================================

/// Updates published while one subscriber never reads. At well over 1 KiB
/// each, they overflow its Unix socket buffer (~200 KiB on Linux) many
/// times over, so its writes are certainly blocked.
const FLOOD_UPDATES: usize = 2000;

/// Bound for work that must not wait on a stuck subscriber. A stalled
/// server never finishes, so any finite bound tells the two apart.
const NOT_STALLED: Duration = Duration::from_secs(30);

/// Connects, handshakes and subscribes to all sessions, consuming the
/// initial session list.
async fn subscribe_client(server: &TestServer, client_id: &str) -> TestClient {
    let mut client = server.connect().await;
    client.handshake(Some(client_id.to_string())).await;
    client.send(ClientMessage::subscribe(None)).await;
    match client.recv().await {
        DaemonMessage::SessionList { .. } => client,
        other => panic!("Expected SessionList, got {other:?}"),
    }
}

/// Subscribes a client that never reads again, then floods updates past
/// its socket buffer while a healthy subscriber reads each one in
/// lockstep. Fails if the stuck subscriber stalls the healthy one.
/// Returns the stuck client so its socket stays open.
async fn flood_past_stuck_subscriber(
    server: &TestServer,
    registry: &atmd::registry::RegistryHandle,
) -> TestClient {
    let stuck = subscribe_client(server, "stuck").await;
    let mut healthy = subscribe_client(server, "healthy").await;

    let session_id = SessionId::new("flood");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    let flood = async {
        for _ in 0..FLOOD_UPDATES {
            registry
                .apply_lifecycle_event(
                    session_id.clone(),
                    LifecycleEvent::PromptSubmit {
                        prompt: Some("x".repeat(1024)),
                    },
                    Harness::ClaudeCode,
                    None,
                    None,
                )
                .await
                .expect("apply lifecycle event");
            match healthy.recv().await {
                DaemonMessage::SessionUpdated { .. } => {}
                other => panic!("Expected SessionUpdated, got {other:?}"),
            }
        }
    };
    assert!(
        timeout(NOT_STALLED, flood).await.is_ok(),
        "healthy subscriber stalled behind a subscriber that never reads"
    );
    stuck
}

/// A subscriber that is briefly busy (reads nothing while a burst is
/// published) still receives every update afterwards instead of being
/// disconnected for lagging. 1500 matches the burst in the real-atmd probe.
#[tokio::test]
async fn test_subscriber_busy_during_burst_is_not_disconnected() {
    const BURST_UPDATES: usize = 1500;
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut busy = subscribe_client(&server, "busy").await;

    let session_id = SessionId::new("burst");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");
    for _ in 0..BURST_UPDATES {
        registry
            .apply_lifecycle_event(
                session_id.clone(),
                LifecycleEvent::PromptSubmit {
                    prompt: Some("x".repeat(1024)),
                },
                Harness::ClaudeCode,
                None,
                None,
            )
            .await
            .expect("apply lifecycle event");
    }

    for i in 0..BURST_UPDATES {
        let mut line = String::new();
        let read = timeout(NOT_STALLED, busy.reader.read_line(&mut line)).await;
        assert!(
            matches!(read, Ok(Ok(n)) if n > 0),
            "disconnected after {i} of {BURST_UPDATES} updates: {read:?}"
        );
    }

    server.shutdown().await;
}

#[tokio::test]
async fn test_stuck_subscriber_does_not_stall_other_subscribers() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let _stuck = flood_past_stuck_subscriber(&server, &registry).await;
    server.shutdown().await;
}

#[tokio::test]
async fn test_subscribe_succeeds_while_a_subscriber_is_stuck() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let _stuck = flood_past_stuck_subscriber(&server, &registry).await;

    let late = timeout(NOT_STALLED, subscribe_client(&server, "late")).await;
    assert!(late.is_ok(), "subscribe stalled behind a stuck subscriber");

    server.shutdown().await;
}

#[tokio::test]
async fn test_shutdown_completes_while_a_subscriber_is_stuck() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let _stuck = flood_past_stuck_subscriber(&server, &registry).await;
    let socket_path = server.socket_path.clone();

    server.cancel_token.cancel();
    let stopped = timeout(NOT_STALLED, server.run_task).await;

    assert!(
        stopped.is_ok(),
        "server.run() stalled behind a stuck subscriber"
    );
    assert!(
        !socket_path.exists(),
        "socket should be removed on shutdown"
    );
}

/// A reply that hits WRITE_TIMEOUT may have left half a line on the
/// socket, so the server closes the connection rather than replying again
/// and carrying on. Real sockets use an unpaused clock; do not drain until
/// the failed-writer event proves the real write timeout happened.
#[tokio::test]
async fn test_connection_closes_after_a_write_timeout() {
    let (_trace, mut failure) = failed_writer_events();
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    // Far more pongs than the socket buffer holds, sent without reading.
    // Writes fail once the server closes, so errors end the loop. The
    // write half stays open afterwards: closing it would send EOF and let
    // the server close for that reason instead.
    let mut writer = client.writer;
    let producer = tokio::spawn(async move {
        for seq in 0..20_000 {
            let json = serde_json::to_string(&ClientMessage::ping(seq)).unwrap();
            if writer
                .write_all(format!("{json}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
        std::future::pending::<()>().await;
    });
    let drained = timeout(
        Duration::from_secs(60),
        drain_failed_writer(&mut client.reader, &mut failure),
    )
    .await;
    assert!(
        drained.is_ok(),
        "connection stayed open after a write timeout"
    );
    producer.abort();
    let _ = producer.await;

    server.shutdown().await;
}

/// The error reply to a bad message can hit WRITE_TIMEOUT too, and may also
/// leave half a line, so it closes the connection like any failed reply.
/// The bad messages lack a session_id, so each one fails in the handler
/// and gets an error reply. Wait for the failed-writer event without reading
/// early or pausing a clock over real socket I/O.
#[tokio::test]
async fn test_connection_closes_when_an_error_reply_times_out() {
    let (_trace, mut failure) = failed_writer_events();
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let bad = ClientMessage {
        protocol_version: ProtocolVersion::CURRENT,
        message: MessageType::StatusUpdate {
            data: serde_json::json!({ "not_session_id": "x" }),
        },
    };
    let line = format!("{}\n", serde_json::to_string(&bad).unwrap());
    let mut writer = client.writer;
    let producer = tokio::spawn(async move {
        for _ in 0..20_000 {
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
        std::future::pending::<()>().await;
    });
    let drained = timeout(
        Duration::from_secs(60),
        drain_failed_writer(&mut client.reader, &mut failure),
    )
    .await;
    assert!(
        drained.is_ok(),
        "connection stayed open after an error reply timed out"
    );
    producer.abort();
    let _ = producer.await;

    server.shutdown().await;
}

/// A client that connects but never sends `Connect` is dropped once the
/// handshake deadline passes. Time is paused, so the deadline elapses
/// instantly once the runtime is otherwise idle.
#[tokio::test(start_paused = true)]
async fn test_handshake_deadline_closes_silent_client() {
    // Handler owns the handshake deadline; paused clocks need in-memory I/O.
    let (server, mut client) = HandlerServer::new(spawn_registry());

    let mut line = String::new();
    let read = timeout(Duration::from_secs(60), client.reader.read_line(&mut line)).await;

    assert!(
        matches!(read, Ok(Ok(0))),
        "expected EOF after the handshake deadline, got {read:?}"
    );
    server.shutdown().await;
}

/// A subscribed TUI sends nothing after `Subscribe`, so the idle read
/// timeout must not drop it. Time is paused, so READ_TIMEOUT (300s)
/// passes instantly while the handler waits on in-memory duplex input.
#[tokio::test(start_paused = true)]
async fn test_subscribed_client_stays_connected_past_read_timeout() {
    // Handler owns subscribed idle exemption; paused clocks need in-memory I/O.
    let (server, mut client) = HandlerServer::new(spawn_registry());
    client.handshake(None).await;
    client.send(ClientMessage::subscribe(None)).await;
    assert!(matches!(
        client.recv().await,
        DaemonMessage::SessionList { .. }
    ));

    let mut line = String::new();
    let read = timeout(Duration::from_secs(600), client.reader.read_line(&mut line)).await;
    assert!(
        read.is_err(),
        "subscribed connection closed while idle: {read:?}"
    );

    // Still served after the idle period.
    client.send(ClientMessage::ping(1)).await;
    assert!(matches!(
        client.recv().await,
        DaemonMessage::Pong { seq: 1 }
    ));

    server.shutdown().await;
}

/// A connection that never subscribed is still closed once idle past
/// READ_TIMEOUT.
#[tokio::test(start_paused = true)]
async fn test_unsubscribed_client_times_out_when_idle() {
    // Handler owns the idle deadline; paused clocks need in-memory I/O.
    let (server, mut client) = HandlerServer::new(spawn_registry());
    client.handshake(None).await;

    let mut line = String::new();
    let read = timeout(Duration::from_secs(600), client.reader.read_line(&mut line)).await;
    assert!(
        matches!(read, Ok(Ok(0))),
        "expected EOF after the idle timeout, got {read:?}"
    );

    server.shutdown().await;
}

/// Unsubscribing puts the idle read timeout back.
#[tokio::test(start_paused = true)]
async fn test_unsubscribed_after_subscribe_times_out_when_idle() {
    // Handler restores idle timeout on Unsubscribe; paused clocks need in-memory I/O.
    let (server, mut client) = HandlerServer::new(spawn_registry());
    client.handshake(None).await;
    client.send(ClientMessage::subscribe(None)).await;
    assert!(matches!(
        client.recv().await,
        DaemonMessage::SessionList { .. }
    ));
    client
        .send(ClientMessage::new(MessageType::Unsubscribe))
        .await;

    let mut line = String::new();
    let read = timeout(Duration::from_secs(600), client.reader.read_line(&mut line)).await;
    assert!(
        matches!(read, Ok(Ok(0))),
        "expected EOF after the idle timeout, got {read:?}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn test_client_disconnect_message() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    // Send disconnect
    client.send(ClientMessage::disconnect()).await;

    // Connection will close (server won't send response to disconnect),
    // rather than answer with an Error "Connection closed"
    let mut line = String::new();
    let read = timeout(Duration::from_secs(5), client.reader.read_line(&mut line)).await;
    assert!(
        matches!(read, Ok(Ok(0))),
        "expected EOF after Disconnect, got {read:?}: {line}"
    );

    server.shutdown().await;
}

// ============================================================================
// Protocol Tests
// ============================================================================

#[tokio::test]
async fn test_ping_pong() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    // Send ping with sequence number
    client.send(ClientMessage::ping(42)).await;

    // Should receive pong with same seq
    match client.recv().await {
        DaemonMessage::Pong { seq } => {
            assert_eq!(seq, 42, "Pong seq should match ping seq");
        }
        other => panic!("Expected Pong, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn test_list_sessions_command() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    // Request session list
    client.send(ClientMessage::list_sessions()).await;

    // Should receive SessionList
    match client.recv().await {
        DaemonMessage::SessionList { sessions } => {
            // Initially empty
            assert_eq!(sessions.len(), 0);
        }
        other => panic!("Expected SessionList, got {other:?}"),
    }

    server.shutdown().await;
}

// ============================================================================
// Error Handling Tests
// ============================================================================

#[tokio::test]
async fn test_wrong_message_before_handshake() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    // Send wrong message type before handshake
    client.send(ClientMessage::list_sessions()).await;

    // Should receive error
    match client.recv().await {
        DaemonMessage::Error { message, .. } => {
            assert!(
                message.contains("Expected Connect"),
                "Error should mention expected Connect message, got: {message}"
            );
        }
        other => panic!("Expected Error, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn test_duplicate_connect_rejected() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    // Try to connect again
    client.send(ClientMessage::connect(None)).await;

    // Should receive error
    match client.recv().await {
        DaemonMessage::Error { message, .. } => {
            assert!(
                message.contains("Already connected"),
                "Error should mention 'Already connected', got: {message}"
            );
        }
        other => panic!("Expected Error, got {other:?}"),
    }

    server.shutdown().await;
}

// ============================================================================
// Concurrent Clients Tests
// ============================================================================

#[tokio::test]
async fn test_multiple_clients_concurrent() {
    let server = TestServer::spawn().await;

    // Spawn 5 clients concurrently
    let mut handles = Vec::new();
    for i in 0..5 {
        let socket_path = server.socket_path.clone();
        let handle = tokio::spawn(async move {
            let stream = UnixStream::connect(&socket_path).await.unwrap();
            let mut client = TestClient::new(stream);

            let id = client.handshake(Some(format!("concurrent-{i}"))).await;
            assert_eq!(id, format!("concurrent-{i}"));

            // Send list request
            client.send(ClientMessage::list_sessions()).await;
            let _ = client.recv().await;
        });
        handles.push(handle);
    }

    // All should succeed
    for handle in handles {
        handle.await.expect("concurrent client task should succeed");
    }

    server.shutdown().await;
}

// ============================================================================
// Hook Event → LifecycleEvent translation (end-to-end)
//
// These tests drive Claude-shaped raw hook-event JSON over the wire and
// confirm that the connection layer translates it into a vendor-neutral
// `LifecycleEvent`, the registry applies it correctly, and the resulting
// session state matches expectations. Together they exercise:
//
//   wire JSON  →  RawHookEvent  →  LifecycleEvent  →  Session state
//
// Each test pre-registers a session (to skip the PID-based create-on-event
// path that needs `/proc` lookups), then sends a hook event and reads the
// session view back through the registry handle.
//
// Synthetic: `hook_event_json` builds minimal hand-written payloads. The
// `TeammateIdle` test below sends a captured one; the adapter's fixture
// tests cover full captured payloads.
// ============================================================================

/// Builds a raw Claude hook-event JSON payload for the given session.
fn hook_event_json(
    session_id: &str,
    event_name: &str,
    extras: serde_json::Value,
) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "session_id": session_id,
        "hook_event_name": event_name,
    });
    if let (Some(map), Some(extras)) = (obj.as_object_mut(), extras.as_object()) {
        for (k, v) in extras {
            map.insert(k.clone(), v.clone());
        }
    }
    obj
}

#[tokio::test]
async fn test_e2e_pre_tool_use_translates_to_tool_call_start() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-tool");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    // Send a Claude PreToolUse(Bash) hook event over the wire.
    client
        .send(ClientMessage::hook_event(hook_event_json(
            session_id.as_str(),
            "PreToolUse",
            serde_json::json!({"tool_name": "Bash"}),
        )))
        .await;

    // Same-stream Pong follows the awaited registry apply, not a settle delay.
    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session should still exist");
    assert_eq!(
        view.status_label, "working",
        "PreToolUse(Bash) should translate to ToolCallStart -> Working"
    );
    assert_eq!(view.activity_detail, Some("Bash".into()));

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_pre_tool_use_interactive_translates_to_needs_input() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-interactive");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    // PreToolUse(AskUserQuestion) is the load-bearing case: the translator
    // must collapse this into NeedsInput rather than ToolCallStart, because
    // AskUserQuestion semantically blocks waiting on the user.
    client
        .send(ClientMessage::hook_event(hook_event_json(
            session_id.as_str(),
            "PreToolUse",
            serde_json::json!({"tool_name": "AskUserQuestion"}),
        )))
        .await;

    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session should still exist");
    assert_eq!(
        view.status_label, "needs input",
        "interactive tool should translate to NeedsInput -> AttentionNeeded"
    );
    assert_eq!(view.activity_detail, Some("AskUserQuestion".into()));

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_stop_translates_to_idle() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-stop");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    // Drive Working -> Idle via the wire. Stop translates to WorkingEnd
    // which transitions the session to Idle.
    client
        .send(ClientMessage::hook_event(hook_event_json(
            session_id.as_str(),
            "PreToolUse",
            serde_json::json!({"tool_name": "Bash"}),
        )))
        .await;
    client.applied(0).await;

    client
        .send(ClientMessage::hook_event(hook_event_json(
            session_id.as_str(),
            "Stop",
            serde_json::json!({}),
        )))
        .await;
    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session should still exist after Stop");
    assert_eq!(
        view.status_label, "idle",
        "Stop should translate to WorkingEnd -> Idle"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_session_end_removes_session() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-end");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    assert!(registry.get_session(session_id.clone()).await.is_some());

    client
        .send(ClientMessage::hook_event(hook_event_json(
            session_id.as_str(),
            "SessionEnd",
            serde_json::json!({"reason": "clear"}),
        )))
        .await;
    client.applied(0).await;

    assert!(
        registry.get_session(session_id.clone()).await.is_none(),
        "SessionEnd lifecycle event should remove the session"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_notification_permission_prompt_becomes_needs_input() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-permission");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    // Claude Notification(permission_prompt) is one of the cases the
    // translation layer collapses to NeedsInput - the same conceptual
    // signal pi's extension synthesizes from `tool_call`.
    client
        .send(ClientMessage::hook_event(hook_event_json(
            session_id.as_str(),
            "Notification",
            serde_json::json!({"notification_type": "permission_prompt"}),
        )))
        .await;
    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session should still exist");
    assert_eq!(
        view.status_label, "needs input",
        "Notification(permission_prompt) should translate to NeedsInput"
    );

    server.shutdown().await;
}

/// A tmux-mode teammate fires `TeammateIdle` from its *own* session. It
/// must idle that session, not spawn a `waiter@<teammate>` child under
/// it. Captured: Claude Code 2.1.288, 2026-10-04, a tmux-mode team run
/// (see atm-claude-adapter's crate docs); paths rewritten to
/// `/home/user` and `/tmp/xxxxxxxxxxx`.
#[tokio::test]
async fn test_e2e_tmux_teammate_idle_from_own_session_adds_no_child() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let teammate = SessionId::new("34d56705-0907-49a6-8785-e72091a93178");
    registry
        .register(create_test_session(teammate.as_str()))
        .await
        .expect("register teammate session");
    // Start working, so the idle assert below proves the hook landed.
    client
        .send(ClientMessage::hook_event(hook_event_json(
            teammate.as_str(),
            "PreToolUse",
            serde_json::json!({"tool_name": "Bash"}),
        )))
        .await;
    client.applied(0).await;
    let working = registry.get_session(teammate.clone()).await;
    assert_eq!(
        working.map(|view| view.status_label),
        Some("working".into())
    );

    let payload = serde_json::json!({
        "session_id": "34d56705-0907-49a6-8785-e72091a93178",
        "transcript_path": "/home/user/.claude/projects/-home-user-project/34d56705-0907-49a6-8785-e72091a93178.jsonl",
        "cwd": "/home/user/project",
        "scratchpad_dir": "/tmp/xxxxxxxxxxx/-home-user-project/34d56705-0907-49a6-8785-e72091a93178/scratchpad",
        "permission_mode": "auto",
        "agent_type": "general-purpose",
        "hook_event_name": "TeammateIdle",
        "teammate_name": "waiter",
        "team_name": "session-ae223ce3"
    });
    client.send(ClientMessage::hook_event(payload)).await;
    client.applied(0).await;

    let ids: Vec<String> = registry
        .get_all_sessions()
        .await
        .into_iter()
        .map(|view| view.id.as_str().to_string())
        .collect();
    assert_eq!(
        ids,
        vec![teammate.as_str().to_string()],
        "no placeholder child"
    );
    let view = registry.get_session(teammate).await.expect("teammate");
    assert_eq!(view.status_label, "idle");
    assert!(view.child_session_ids.is_empty());

    server.shutdown().await;
}

/// atmd's fixture holding every hook payload and status line of one Claude
/// Code TUI session, in the order atm-hook received them. Each line is
/// `{"offset_ms":..,"kind":"hook"|"status_line","payload":..}`, with
/// `offset_ms` from the first call. Captured: Claude Code 2.1.310,
/// 2026-10-24, the run described under "Captured payloads" in
/// atm-claude-adapter's crate docs; paths rewritten to `/home/user` and
/// `/tmp/xxxxxxxxxxx`. The payloads are those of atm-claude-adapter's
/// `claude_tui_session_2.1.310.jsonl` and
/// `claude_status_line_2.1.310.jsonl`; this file adds their order.
const CLAUDE_INTERLEAVED: &str = "tests/fixtures/claude_tui_interleaved_2.1.310.jsonl";

/// Index in [`CLAUDE_INTERLEAVED`] of the first call from the second
/// process, which resumed the session with `--resume` after the first
/// process exited.
const CLAUDE_RESUMED_AT: usize = 42;

/// The `(kind, payload)` of each call in [`CLAUDE_INTERLEAVED`], each
/// payload given a live pid as atm-hook would add one: this test's own for
/// the first process, and its parent's from [`CLAUDE_RESUMED_AT`] on, so
/// atmd sees two processes as it did in the capture.
fn captured_claude_calls() -> Vec<(String, serde_json::Value)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CLAUDE_INTERLEAVED);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    text.lines()
        .enumerate()
        .map(|(i, line)| {
            let mut call: serde_json::Value = serde_json::from_str(line).expect("fixture line");
            let kind = call["kind"].as_str().expect("kind").to_string();
            let mut payload = call["payload"].take();
            payload["pid"] = if i < CLAUDE_RESUMED_AT {
                std::process::id()
            } else {
                std::os::unix::process::parent_id()
            }
            .into();
            (kind, payload)
        })
        .collect()
}

/// Sends one message, then a ping, and returns once the Pong arrives.
/// atmd handles one connection's messages in order and awaits the
/// registry for each, so the Pong means the message has been applied.
async fn send_applied<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    client: &mut TestClient<R, W>,
    seq: u64,
    message: ClientMessage,
) {
    client.send(message).await;
    client.applied(seq).await;
}

#[test]
fn test_claude_interleaved_fixture_matches_its_sources() {
    // Every payload is byte-identical to its line in the two source
    // fixtures, so the interleaved fixture adds only the capture order.
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let hooks_path =
        crates.join("atm-claude-adapter/tests/fixtures/claude_tui_session_2.1.310.jsonl");
    let status_path =
        crates.join("atm-claude-adapter/tests/fixtures/claude_status_line_2.1.310.jsonl");
    if !hooks_path.exists() || !status_path.exists() {
        // atmd's tests ship in its crates.io package without those crates.
        eprintln!("skipped: no source fixtures outside the workspace");
        return;
    }
    let read = |path: &PathBuf| {
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    };
    let text = read(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CLAUDE_INTERLEAVED));
    let mut hooks = Vec::new();
    let mut status_lines = Vec::new();
    for line in text.lines() {
        let (head, payload) = line.split_once(r#","payload":"#).expect("payload field");
        let payload = payload
            .strip_suffix('}')
            .expect("payload is the last field");
        if head.ends_with(r#""kind":"hook""#) {
            hooks.push(payload);
        } else if head.ends_with(r#""kind":"status_line""#) {
            status_lines.push(payload);
        } else {
            panic!("unexpected kind in {head}");
        }
    }
    assert_eq!(hooks, read(&hooks_path).lines().collect::<Vec<_>>());
    assert_eq!(status_lines, read(&status_path).lines().collect::<Vec<_>>());
}

#[tokio::test]
async fn test_e2e_claude_captured_status_lines_leave_status_alone() {
    // Claude 2.1.310 sends a status line ~300 ms after every Stop, and
    // after `/compact`, `/clear` and a permission-mode change at the idle
    // prompt. Status lines carry no turn state, so none of them may change
    // a row's status or activity, and above all none may turn an idle row
    // back to working.
    //
    // The calls go over one connection in capture order. atm-hook opens
    // a connection per call, so this replay doesn't cover calls arriving
    // out of order.
    let calls = captured_claude_calls();
    assert_eq!(calls.len(), 46);
    assert_eq!(calls[CLAUDE_RESUMED_AT + 1].1["source"], "resume");
    // Each status line that follows an idle point, by its fixture index,
    // with the hook event before it: the permission-mode change at the
    // idle prompt (40 s after SessionStart), the three Stops, `/compact`'s
    // SessionStart, and `/clear`'s SessionStart under a new session id.
    let checkpoints = [
        (2, "SessionStart"),
        (10, "Stop"),
        (18, "Stop"),
        (29, "Stop"),
        (35, "SessionStart"),
        (39, "SessionStart"),
    ];

    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;
    let state = |view: Option<SessionView>| view.map(|v| (v.status_label, v.activity_detail));
    let mut seen = Vec::new();
    let mut changed = Vec::new();
    let mut dropped = Vec::new();
    for (seq, (i, (kind, payload))) in (1..).zip(calls.iter().enumerate()) {
        let message = match kind.as_str() {
            "hook" => ClientMessage::hook_event(payload.clone()),
            "status_line" => ClientMessage::status_update(payload.clone()),
            other => panic!("unexpected kind {other}"),
        };
        let id = SessionId::new(payload["session_id"].as_str().expect("session_id"));
        let before = state(registry.get_session(id.clone()).await);
        send_applied(&mut client, seq, message).await;
        let after = state(registry.get_session(id).await);
        if kind == "status_line" {
            let unchanged = match (&before, &after) {
                (Some(before), Some(after)) => after == before,
                // Each process's first status line creates its row, which
                // starts idle: call 0, before SessionStart(startup), and
                // call 42, from the resumed process's new pid.
                (None, Some((label, _))) => label == "idle",
                // A late status line for a session that SessionEnd just
                // removed in the same process is dropped.
                (None, None) => {
                    dropped.push(i);
                    true
                }
                _ => false,
            };
            if !unchanged {
                changed.push((i, before, after.clone()));
            }
        }
        if let Some(&(_, previous)) = checkpoints.iter().find(|(at, _)| *at == i) {
            let hook = calls[i - 1].1["hook_event_name"].as_str();
            assert_eq!(hook, Some(previous), "fixture order at {i}");
            seen.push((i, kind.as_str(), after.map(|(label, _)| label)));
        }
    }
    assert!(
        changed.is_empty(),
        "status lines changed rows: {changed:#?}"
    );
    // The lines that followed SessionEnd: /clear, /exit, and the resumed
    // session's /exit.
    assert_eq!(dropped, [37, 41, 45]);
    let expected: Vec<_> = checkpoints
        .iter()
        .map(|&(i, _)| (i, "status_line", Some("idle".to_string())))
        .collect();
    assert_eq!(seen, expected);

    server.shutdown().await;
}

/// Characterize committed parent state, including metadata-only traffic between
/// steps. No readiness/shutdown sleeps: biased join polls server.run() through
/// its synchronous bind before connecting; Pong and actor replies order writes.
async fn assert_claude_boundary_steps(events: &[serde_json::Value], steps: &[(usize, &str)]) {
    let temp = tempfile::tempdir().expect("boundary server dir");
    let socket = temp.path().join("boundary.sock");
    let registry = spawn_registry();
    let cancel = CancellationToken::new();
    let server = DaemonServer::new(socket.clone(), registry.clone(), cancel.clone());
    let id = SessionId::new(events[steps[0].0]["session_id"].as_str().unwrap());
    // Synthetic: retarget a real 2.1.310 metadata frame after each live row
    // checkpoint. This checks that metadata cannot substitute for completion;
    // it makes no claim about the real 2.1.325 status-line timing or schema.
    let Some(statuses) =
        captured_hook_payloads("atm-claude-adapter", "claude_status_line_2.1.310.jsonl")
    else {
        return;
    };
    let mut status = statuses[0].clone();
    status["session_id"] = id.as_str().into();
    status["cwd"] = events[steps[0].0]["cwd"].clone();
    let replay = async {
        let stream = UnixStream::connect(&socket).await.expect("bound server");
        let mut client = TestClient::new(stream);
        client.handshake(None).await;
        let mut first_prompt = None;
        for (seq, &(index, expected)) in (1_u64..).zip(steps) {
            let payload = &events[index];
            assert_eq!(payload["session_id"], id.as_str());
            if payload["hook_event_name"] == "UserPromptSubmit" && first_prompt.is_none() {
                first_prompt = payload["prompt"].as_str();
            }
            send_applied(
                &mut client,
                seq * 2,
                ClientMessage::hook_event(payload.clone()),
            )
            .await;
            let view = registry.get_session(id.clone()).await;
            assert_eq!(
                view.as_ref()
                    .map_or("absent", |view| view.status_label.as_str()),
                expected,
                "{} step {index}: {}",
                id.as_str(),
                payload["hook_event_name"],
            );
            if let Some(view) = view {
                assert_eq!(
                    view.first_prompt.as_deref(),
                    first_prompt,
                    "first prompt is not replaced by wake/compaction"
                );
                send_applied(
                    &mut client,
                    seq * 2 + 1,
                    ClientMessage::status_update(status.clone()),
                )
                .await;
                assert_eq!(
                    registry.get_session(id.clone()).await.unwrap().status_label,
                    expected,
                    "metadata cannot change the parent phase at step {index}"
                );
            }
        }
        drop(client);
        cancel.cancel();
    };
    let (result, ()) = timeout(Duration::from_secs(60), async {
        tokio::join!(biased; server.run(), replay)
    })
    .await
    .expect("boundary replay hang guard");
    result.expect("boundary server shutdown");
}

#[tokio::test]
async fn test_e2e_claude_captured_turn_boundaries() {
    // Captured: Claude Code 2.1.325, 2026-11-16, sealed rig stub mode.
    // See adapter crate docs for the compact-only flag exception, actual
    // request-body capability proof, and teardown SessionEnd provenance.
    let Some(events) =
        captured_hook_payloads("atm-claude-adapter", "claude_turn_boundaries_2.1.325.jsonl")
    else {
        return;
    };
    let cases: &[(&str, &[&str])] = &[
        (
            "normal-session",
            &["idle", "working", "working", "working", "idle", "absent"],
        ),
        (
            "blocked-session",
            &[
                "idle", "working", "idle", "working", "working", "idle", "absent",
            ],
        ),
        (
            "async-session",
            &[
                "idle", "working", "working", "working", "idle", "working", "idle", "absent",
            ],
        ),
        (
            "compact-session",
            &[
                "idle", "working", "working", "working", "idle", "working", "working", "working",
                "idle", "idle", "absent",
            ],
        ),
    ];
    for &(id, expected) in cases {
        let calls: Vec<_> = events
            .iter()
            .filter(|event| event["session_id"] == id)
            .cloned()
            .collect();
        assert_eq!(calls.len(), expected.len(), "{id}");
        if id == "blocked-session" {
            assert_eq!(calls[1]["prompt_id"], calls[2]["prompt_id"]);
            assert_eq!(calls[2]["prompt_id"], calls[3]["prompt_id"]);
            assert_eq!(calls[3]["prompt_id"], calls[5]["prompt_id"]);
        }
        if id == "async-session" {
            assert_ne!(calls[1]["prompt_id"], calls[5]["prompt_id"]);
            assert_eq!(calls[5]["prompt_id"], calls[6]["prompt_id"]);
        }
        if id == "compact-session" {
            assert_ne!(calls[1]["prompt_id"], calls[5]["prompt_id"]);
            assert_eq!(calls[5]["prompt_id"], calls[8]["prompt_id"]);
            assert_eq!(calls[8]["prompt_id"], calls[9]["prompt_id"]);
        }
        let steps: Vec<_> = expected.iter().copied().enumerate().collect();
        assert_claude_boundary_steps(&calls, &steps).await;
    }
}

#[tokio::test]
async fn late_parent_tool_after_stop_reopens_working() {
    // Captured: Claude Code 2.1.310, 2026-10-24, sealed TUI/fake Messages API
    // (adapter crate docs). Synthetic: reorder parent tool ends/starts after
    // Stop; these were NOT captured arriving late after an irrevocable end.
    let Some(old) =
        captured_hook_payloads("atm-claude-adapter", "claude_tui_session_2.1.310.jsonl")
    else {
        return;
    };
    assert_eq!(old[1]["prompt_id"], old[6]["prompt_id"]);
    assert_eq!(old[2]["tool_use_id"], old[5]["tool_use_id"]);
    let cases: &[&[(usize, &str)]] = &[
        &[
            (0, "idle"),
            (1, "working"),
            (2, "working"),
            (6, "idle"),
            (5, "working"),
        ],
        &[
            (0, "idle"),
            (7, "working"),
            (8, "working"),
            (12, "idle"),
            (11, "working"),
        ],
        &[(0, "idle"), (1, "working"), (6, "idle"), (2, "working")],
        // Synthetic child control: tagged child activity after a parent's
        // Stop must not reactivate the parent, unlike an ordinary tool end.
        &[
            (0, "idle"),
            (18, "idle"),
            (17, "idle"),
            (19, "idle"),
            (20, "idle"),
        ],
    ];
    for steps in cases {
        assert_claude_boundary_steps(&old, steps).await;
    }
    // Captured: Claude Code 2.1.325, 2026-11-16, sealed rig stub mode.
    // Synthetic: deliver this normal turn's PostToolUse after its Stop.
    let Some(current) =
        captured_hook_payloads("atm-claude-adapter", "claude_turn_boundaries_2.1.325.jsonl")
    else {
        return;
    };
    assert_claude_boundary_steps(
        &current,
        &[
            (0, "idle"),
            (1, "working"),
            (2, "working"),
            (4, "idle"),
            (3, "working"),
        ],
    )
    .await;
}

#[tokio::test]
async fn wake_requires_its_own_terminal() {
    // Captured: Claude Code 2.1.310, 2026-10-24, sealed TUI/fake Messages API.
    // Synthetic: omit the wake terminal while status/old child traffic arrives,
    // then deliver it. The original historical wake did have this final Stop.
    let Some(old) =
        captured_hook_payloads("atm-claude-adapter", "claude_tui_session_2.1.310.jsonl")
    else {
        return;
    };
    assert!(old[21]["prompt"]
        .as_str()
        .unwrap()
        .starts_with("<task-notification>"));
    assert_ne!(old[18]["prompt_id"], old[21]["prompt_id"]);
    assert_eq!(old[21]["prompt_id"], old[22]["prompt_id"]);
    assert_claude_boundary_steps(
        &old,
        &[
            (0, "idle"),
            (18, "idle"),
            (21, "working"),
            (20, "working"),
            (22, "idle"),
        ],
    )
    .await;
    // Captured: Claude Code 2.1.325, 2026-11-16, sealed rig stub mode.
    // Synthetic: withhold the real configured asyncRewake turn's terminal
    // until metadata has demonstrably left its new prompt Working.
    let Some(current) =
        captured_hook_payloads("atm-claude-adapter", "claude_turn_boundaries_2.1.325.jsonl")
    else {
        return;
    };
    let calls: Vec<_> = current
        .into_iter()
        .filter(|event| event["session_id"] == "async-session")
        .collect();
    assert_claude_boundary_steps(
        &calls,
        &[
            (0, "idle"),
            (1, "working"),
            (4, "idle"),
            (5, "working"),
            (6, "idle"),
        ],
    )
    .await;
}

#[tokio::test]
async fn manual_compact_without_completion_stays_working() {
    // Captured: Claude Code 2.1.310, 2026-10-24, sealed TUI/fake Messages API.
    // Synthetic: omit SessionStart until after compactor/status traffic.
    let Some(old) =
        captured_hook_payloads("atm-claude-adapter", "claude_tui_session_2.1.310.jsonl")
    else {
        return;
    };
    assert_claude_boundary_steps(
        &old,
        &[(0, "idle"), (24, "working"), (25, "working"), (26, "idle")],
    )
    .await;
    // Captured: Claude Code 2.1.325, 2026-11-16, sealed rig stub mode.
    // Synthetic: withhold compact SessionStart, deliver the real PostCompact,
    // then release SessionStart. Today PostCompact is suppressed, not Idle.
    let Some(current) =
        captured_hook_payloads("atm-claude-adapter", "claude_turn_boundaries_2.1.325.jsonl")
    else {
        return;
    };
    let calls: Vec<_> = current
        .into_iter()
        .filter(|event| event["session_id"] == "compact-session")
        .collect();
    assert_claude_boundary_steps(
        &calls,
        &[
            (0, "idle"),
            (5, "working"),
            (6, "working"),
            (7, "working"),
            (9, "working"),
            (8, "idle"),
        ],
    )
    .await;
}

#[tokio::test]
async fn test_e2e_claude_captured_blank_model_status_line_creates_row() {
    // Captured: Claude Code 2.1.310, 2026-10-25, the first status line of
    // a TUI started with `--model " "` (see atm-claude-adapter's
    // status_line.rs docs).
    // Claude reports the blank model as `"id": ""`. The session is real,
    // so the line creates its row, as Unknown, even with no hook before
    // it (`--bare` sends status lines only).
    let fixture = "claude_status_line_blank_model_2.1.310.json";
    let Some(lines) = captured_hook_payloads("atm-claude-adapter", fixture) else {
        return;
    };
    let [line] = lines.as_slice() else {
        panic!("{fixture} holds one line");
    };
    assert_eq!(line["model"]["id"], "");

    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;
    send_applied(&mut client, 1, ClientMessage::status_update(line.clone())).await;

    let id = SessionId::new(line["session_id"].as_str().expect("session_id"));
    let view = registry
        .get_session(id)
        .await
        .expect("the line creates the row");
    assert_eq!(view.model, "Unknown");
    assert_eq!(view.harness, "claude");

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_status_line_without_model_creates_row_and_empty_id_keeps_model() {
    // Synthetic: a status line with a pid creates its row even without a
    // model, and Claude's blank model (`"id": ""`, as captured above)
    // keeps the model an earlier line reported.
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;
    let id = SessionId::new("e2e-status-line-model");
    let pid = std::process::id();

    let no_model = serde_json::json!({"session_id": id.as_str(), "pid": pid});
    send_applied(&mut client, 1, ClientMessage::status_update(no_model)).await;
    let view = registry
        .get_session(id.clone())
        .await
        .expect("a line without a model creates the row");
    assert_eq!(view.model, "Unknown");

    let opus = serde_json::json!({
        "session_id": id.as_str(),
        "pid": pid,
        "model": {"id": "claude-opus-5-5", "display_name": "Opus 5.5"}
    });
    send_applied(&mut client, 2, ClientMessage::status_update(opus)).await;
    let view = registry.get_session(id.clone()).await.expect("row");
    assert_eq!(view.model, "Opus 5.5");

    let blank = serde_json::json!({
        "session_id": id.as_str(),
        "pid": pid,
        "model": {"id": "", "display_name": "unknown model"}
    });
    send_applied(&mut client, 3, ClientMessage::status_update(blank)).await;
    let view = registry.get_session(id).await.expect("row");
    assert_eq!(view.model, "Opus 5.5");

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_status_line_session_id_comes_from_the_line() {
    // Synthetic: status lines whose session_id is missing or not a string,
    // or that aren't JSON objects, get a parse error reply and the
    // connection keeps serving. A string id, even an empty one, creates
    // the row.
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;
    let pid = std::process::id();

    // serde can fill a struct from an array by field position. The second
    // array puts the pid at RawStatusLine's pid, field 9, so on its own it
    // would parse and create a row.
    let mut array_with_pid = vec![serde_json::json!("e2e-array-id")];
    array_with_pid.extend((0..8).map(|_| serde_json::Value::Null));
    array_with_pid.push(serde_json::json!(pid));
    for bad in [
        serde_json::json!({"pid": pid}),
        serde_json::json!({"session_id": 7, "pid": pid}),
        serde_json::json!({"session_id": null, "pid": pid}),
        serde_json::json!("x"),
        serde_json::json!(["e2e-array-id"]),
        serde_json::Value::Array(array_with_pid),
    ] {
        client.send(ClientMessage::status_update(bad.clone())).await;
        let reply = timeout(NOT_STALLED, client.recv()).await;
        match reply {
            Ok(DaemonMessage::Error { message, .. }) => {
                assert!(message.starts_with("Parse error: "), "{bad}: {message:?}");
            }
            other => panic!("{bad}: expected a parse error, got {other:?}"),
        }
    }
    assert!(registry
        .get_session(SessionId::new("e2e-array-id"))
        .await
        .is_none());
    server.shutdown().await;

    // A fresh server per id, so each line creates its row
    for id in ["e2e-status-line-id", ""] {
        let (server, registry) = TestServer::spawn_with_registry().await;
        let mut client = server.connect().await;
        client.handshake(None).await;
        let line = serde_json::json!({"session_id": id, "pid": pid});
        send_applied(&mut client, 1, ClientMessage::status_update(line)).await;
        let view = registry
            .get_session(SessionId::new(id))
            .await
            .unwrap_or_else(|| panic!("no row for {id:?}"));
        assert_eq!(view.id.as_str(), id);
        server.shutdown().await;
    }
}

#[tokio::test]
async fn test_e2e_claude_captured_stop_failure_replay() {
    // Captured: Claude Code 2.1.310, 2026-10-25, three TUI turns ended by
    // API errors (see atm-claude-adapter's crate docs). Claude sends
    // StopFailure instead of Stop, then idle_prompt 60 s later, so the
    // row must go idle at StopFailure rather than stay working until then.
    let fixture = "claude_stop_failure_2.1.310.jsonl";
    let Some(events) = captured_hook_payloads("atm-claude-adapter", fixture) else {
        return;
    };
    let sessions: Vec<_> = events.chunks(5).collect();
    assert_eq!(sessions.len(), 3, "{fixture}");
    for session in sessions {
        let names: Vec<_> = session
            .iter()
            .map(|event| event["hook_event_name"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            names,
            [
                "SessionStart",
                "UserPromptSubmit",
                "StopFailure",
                "Notification",
                "SessionEnd"
            ]
        );

        let (server, registry) = TestServer::spawn_with_registry().await;
        let mut client = server.connect().await;
        client.handshake(None).await;
        let mut seen = Vec::new();
        for (seq, event) in (1..).zip(session) {
            send_applied(&mut client, seq, ClientMessage::hook_event(event.clone())).await;
            let states: Vec<_> = registry
                .get_all_sessions()
                .await
                .into_iter()
                .map(|s| (s.status_label, s.activity_detail))
                .collect();
            seen.push(states);
        }
        // SessionStart, UserPromptSubmit, StopFailure, idle_prompt, then
        // the SessionEnd from tearing the capture down, which removes it.
        let state = |label: &str| vec![(label.to_string(), None)];
        let expected = [
            state("idle"),
            state("working"),
            state("idle"),
            state("idle"),
            vec![],
        ];
        assert_eq!(seen, expected, "session {}", session[0]["session_id"]);
        server.shutdown().await;
    }
}

#[tokio::test]
async fn test_e2e_claude_captured_background_agent_dialog_replay() {
    // Captured: Claude Code 2.1.310, 2026-10-25, two TUI turns whose
    // permission dialog stayed open while background subagents started
    // and stopped (see atm-claude-adapter's crate docs). The lead sends no
    // hook of its own while its dialog waits, so a child event that turned
    // it working hid the dialog until the user answered it.
    let fixture = "claude_background_agent_dialog_2.1.310.jsonl";
    let Some(events) = captured_hook_payloads("atm-claude-adapter", fixture) else {
        return;
    };
    assert_eq!(events.len(), 34, "{fixture}");
    let (race, deep) = events.split_at(14);
    // The lead's row after each hook: status, activity, needs_attention,
    // and how many child rows it has.
    let row = |status: &str, activity: Option<&str>, children: usize| {
        let needs_attention = status == "needs input";
        Some((
            status.to_string(),
            activity.map(String::from),
            needs_attention,
            children,
        ))
    };
    let idle = |children| row("idle", None, children);
    let working = |activity, children| row("working", activity, children);
    let asking = |activity, children| row("needs input", Some(activity), children);
    let (agent, thinking) = (Some("Agent"), Some("Thinking"));
    let (bash, permission) = ("Bash: touch atm-probe.txt && echo atm-probe", "Permission");
    let sessions = [
        // A background agent, then a gated Bash call; the agent stops
        // while the dialog is open.
        (
            race,
            vec![
                ("SessionStart", idle(0)),
                ("UserPromptSubmit", working(None, 0)),
                ("PreToolUse", working(agent, 0)),
                ("SubagentStart", working(agent, 1)),
                ("PostToolUse", working(thinking, 1)),
                ("PreToolUse", working(Some("Bash"), 1)),
                ("PermissionRequest", asking(bash, 1)),
                ("Notification", asking(permission, 1)),
                ("SubagentStop", asking(permission, 0)),
                ("PostToolUse", working(thinking, 0)),
                ("UserPromptSubmit", working(None, 0)),
                ("Stop", idle(0)),
                ("Notification", idle(0)),
                ("SessionEnd", None),
            ],
        ),
        // The background agent spawns a nested one during the dialog;
        // Claude restarts it when the nested agent finishes. Its own
        // tool calls carry its agent_id and go to its row.
        (
            deep,
            vec![
                ("SessionStart", idle(0)),
                ("UserPromptSubmit", working(None, 0)),
                ("PreToolUse", working(agent, 0)),
                ("SubagentStart", working(agent, 1)),
                ("PostToolUse", working(thinking, 1)),
                ("PreToolUse", working(Some("Bash"), 1)),
                ("PermissionRequest", asking(bash, 1)),
                ("Notification", asking(permission, 1)),
                ("PreToolUse", asking(permission, 1)),
                ("SubagentStart", asking(permission, 2)),
                ("PostToolUse", asking(permission, 2)),
                ("SubagentStop", asking(permission, 1)),
                ("SubagentStop", asking(permission, 0)),
                ("SubagentStart", asking(permission, 1)),
                ("SubagentStop", asking(permission, 0)),
                ("PostToolUse", working(thinking, 0)),
                ("UserPromptSubmit", working(None, 0)),
                ("Stop", idle(0)),
                ("Notification", idle(0)),
                ("SessionEnd", None),
            ],
        ),
    ];
    for (session, expected) in sessions {
        let lead = SessionId::new(session[0]["session_id"].as_str().expect("session_id"));
        let (server, registry) = TestServer::spawn_with_registry().await;
        let mut client = server.connect().await;
        client.handshake(None).await;
        let mut seen = Vec::new();
        for (seq, event) in (1..).zip(session) {
            send_applied(&mut client, seq, ClientMessage::hook_event(event.clone())).await;
            let row = registry.get_session(lead.clone()).await.map(|view| {
                (
                    view.status_label,
                    view.activity_detail,
                    view.needs_attention,
                    view.child_session_ids.len(),
                )
            });
            let name = event["hook_event_name"].as_str().unwrap_or_default();
            seen.push((name, row));
        }
        assert_eq!(seen, expected, "session {lead}");
        server.shutdown().await;
    }
}

// ============================================================================
// Pi Event → LifecycleEvent translation (end-to-end, wire-level)
//
// Symmetric with the Claude e2e block above. These tests drive
// pi-shaped JSON through MessageType::PiEvent and confirm the
// connection layer hands off to atm-pi-adapter and produces the
// expected session state. They are the wire-level counterpart to
// the unit tests in atm-pi-adapter::translate.
//
// Synthetic: payloads hand-built with `pi_event_json`, except the
// `test_e2e_pi_captured_*` tests, which replay atm-pi-adapter's real pi
// 0.73.1 captures (tests/fixtures/*_0.73.1.jsonl) over the socket.
// ============================================================================

/// Captured: pi 1.5.2, 2026-11-16, sealed interactive PTY/local static
/// provider, extension callbacks and private atmd socket proxy.
/// Actual changed-forwarder frames; IDs/PIDs are anonymized/replaced, not
/// lifecycle fields. Each ListSessions reply is a processing barrier.
#[tokio::test]
async fn pi_1_5_2_terminal_sequences() {
    use atmd::server::{methods::MethodRouter, ConnectionHandler};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
    use tokio::sync::RwLock;

    async fn send(writer: &mut WriteHalf<DuplexStream>, message: ClientMessage) {
        writer
            .write_all(serde_json::to_string(&message).unwrap().as_bytes())
            .await
            .unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.flush().await.unwrap();
    }
    async fn recv(reader: &mut BufReader<ReadHalf<DuplexStream>>) -> serde_json::Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    timeout(Duration::from_secs(30), async {
        let registry = spawn_registry();
        let (server, client) = tokio::io::duplex(65536);
        let (read, write) = tokio::io::split(server);
        let cancel = CancellationToken::new();
        let handler = ConnectionHandler::new(
            read,
            write,
            registry,
            Arc::new(RwLock::new(HashMap::new())),
            1,
            cancel.clone(),
            Arc::new(MethodRouter::default()),
        );
        let task = tokio::spawn(handler.run());
        let (read, mut write) = tokio::io::split(client);
        let mut read = BufReader::new(read);
        send(&mut write, ClientMessage::connect(None)).await;
        assert_eq!(recv(&mut read).await["type"], "connected");
        let adapter = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../atm-pi-adapter");
        if !adapter.join("Cargo.toml").exists() {
            // Published atmd packages do not contain the sibling adapter fixtures.
            cancel.cancel();
            task.await.unwrap();
            return;
        }
        let path = adapter.join("tests/fixtures/pi_turn_frames_1.5.2.jsonl");
        let mut checked = 0;
        for line in std::fs::read_to_string(path).unwrap().lines() {
            let frame: serde_json::Value = serde_json::from_str(line).unwrap();
            let mut data = frame["data"].clone();
            let name = data["event"].as_str().unwrap().to_owned();
            assert_ne!(
                name, "agent_end",
                "modern forwarder must wait for settlement"
            );
            let expected = match name.as_str() {
                "session_start" | "agent_settled" => Some("idle"),
                "session_shutdown" => None,
                "input" | "agent_start" | "session_before_compact" => Some("working"),
                "session_compact" | "session_compact_failed" => {
                    Some(if data["payload"]["reason"] == "manual" {
                        "idle"
                    } else {
                        "working"
                    })
                }
                // Usage events preserve the current working state. The modern
                // forwarder intentionally does not send low-level agent_end.
                "context" | "turn_end" => Some("working"),
                _ => continue,
            };
            let id = data["session_id"].clone();
            data["pid"] = std::process::id().into();
            send(&mut write, ClientMessage::pi_event(data)).await;
            // The response on this same stream is a processing barrier even
            // for suppressed outcomes, not a poll or readiness sleep.
            send(&mut write, ClientMessage::new(MessageType::ListSessions)).await;
            let response = recv(&mut read).await;
            let rows = response["sessions"].as_array().unwrap();
            match expected {
                Some(status) => {
                    assert_eq!(rows.len(), 1, "{name}: {response}");
                    let row = rows.first().unwrap();
                    assert_eq!(row["id"], id, "{name}: new/reload identity");
                    assert_eq!(row["status"], status, "{name}: {response}");
                }
                None => assert!(rows.is_empty(), "{name}: {response}"),
            }
            checked += 1;
        }
        assert!(checked > 50);
        cancel.cancel();
        task.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn pi_1_5_2_idle_registration() {
    // Synthetic until the changed-forwarder zero-provider proof is captured.
    use atmd::server::{methods::MethodRouter, ConnectionHandler};
    use std::{collections::HashMap, sync::Arc};
    use tokio::sync::RwLock;
    timeout(Duration::from_secs(30), async {
        let registry = spawn_registry();
        let pid = std::process::id();
        registry
            .register_discovered(
                SessionId::pending_from_pid(pid),
                pid,
                std::env::temp_dir(),
                Some("%7".into()),
                atm_core::Harness::Pi,
            )
            .await
            .unwrap();
        let (server, client) = tokio::io::duplex(65536);
        let (read, write) = tokio::io::split(server);
        let cancel = CancellationToken::new();
        let handler = ConnectionHandler::new(
            read,
            write,
            registry.clone(),
            Arc::new(RwLock::new(HashMap::new())),
            1,
            cancel.clone(),
            Arc::new(MethodRouter::default()),
        );
        let task = tokio::spawn(handler.run());
        let mut peer = BufReader::new(client);
        async fn send(peer: &mut BufReader<tokio::io::DuplexStream>, frame: ClientMessage) {
            peer.get_mut()
                .write_all((serde_json::to_string(&frame).unwrap() + "\n").as_bytes())
                .await
                .unwrap();
        }
        async fn recv(peer: &mut BufReader<tokio::io::DuplexStream>) -> serde_json::Value {
            let mut line = String::new();
            assert_ne!(peer.read_line(&mut line).await.unwrap(), 0);
            serde_json::from_str(&line).unwrap()
        }
        send(&mut peer, ClientMessage::connect(None)).await;
        assert_eq!(recv(&mut peer).await["type"], "connected");
        for id in ["recovered-session", "other-real-session"] {
            send(
                &mut peer,
                ClientMessage::pi_event(serde_json::json!({
                    "event":"atm_idle", "payload":{}, "session_id":id, "pid":pid,
                    "tmux_pane":"%7", "model":{"id":"probe", "provider":"synthetic"}
                })),
            )
            .await;
            send(&mut peer, ClientMessage::list_sessions()).await;
            let response = recv(&mut peer).await;
            let rows = response["sessions"].as_array().unwrap();
            assert_eq!(rows.len(), 1);
            let row = rows.first().unwrap();
            assert_eq!(
                row["id"], "recovered-session",
                "upgrade pending, never replace a real id"
            );
            assert_eq!(row["status"], "idle");
            assert_eq!(row["model"], "probe");
            assert!(row["first_prompt"].is_null());
        }
        let rows = registry.pane_sessions("%7").await.unwrap();
        assert!(rows.first().unwrap().start.is_none());
        cancel.cancel();
        task.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn pi_1_5_2_dialog_idle_resolution() {
    // Synthetic: proposed dialog measurements, busy/legacy/input-latched twins.
    // Same-stream replies prove processing; no TestServer sleeps or polling.
    use atmd::{
        org::{
            message_evidence::{AttemptScope, MessageEvidence},
            messages::MessageClock,
        },
        server::{methods::MethodRouter, ConnectionHandler},
    };
    use std::{collections::HashMap, sync::Arc};
    use tokio::sync::RwLock;

    async fn send(peer: &mut BufReader<tokio::io::DuplexStream>, frame: ClientMessage) {
        peer.get_mut()
            .write_all((serde_json::to_string(&frame).unwrap() + "\n").as_bytes())
            .await
            .unwrap();
    }
    async fn recv(peer: &mut BufReader<tokio::io::DuplexStream>) -> serde_json::Value {
        let mut line = String::new();
        assert_ne!(peer.read_line(&mut line).await.unwrap(), 0);
        serde_json::from_str(&line).unwrap()
    }
    timeout(Duration::from_secs(30), async {
        let registry = spawn_registry();
        let pid = std::process::id();
        let session = SessionId::new("dialog-session");
        registry.register_discovered(session.clone(), pid, std::env::temp_dir(), Some("%7".into()), Harness::Pi).await.unwrap();
        let evidence = MessageEvidence::new(MessageClock::new(
            tokio::time::Instant::now(), chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        ));
        let message_id = "m123".parse().unwrap();
        let reference = "[m123] Read: atm inbox --id m123";
        evidence.register(message_id, reference.into()).unwrap();
        evidence.arm(AttemptScope {
            message_id, attempt_seq: 124, recipient: "alpha/worker".parse().unwrap(),
            pane: atmd::control::reservation::PaneId::parse("%7").unwrap(),
            harness: Harness::Pi, session_id: Some(session.clone()), excluded_sessions: Default::default(),
        }).unwrap();
        let (server, client) = tokio::io::duplex(65536);
        let (read, write) = tokio::io::split(server);
        let cancel = CancellationToken::new();
        let handler = ConnectionHandler::new(read, write, registry.clone(), Arc::new(RwLock::new(HashMap::new())),
            1, cancel.clone(), Arc::new(MethodRouter::default()))
            .with_message_evidence(Some(evidence.clone()), None);
        let task = tokio::spawn(handler.run());
        let mut peer = BufReader::new(client);
        send(&mut peer, ClientMessage::connect(None)).await;
        assert_eq!(recv(&mut peer).await["type"], "connected");
        let mut cases = Vec::new();
        for (event, payload, expected) in [
            ("atm_needs_input_open", serde_json::json!({"title":reference}), "attention_needed"),
            ("atm_needs_input_resolved", serde_json::json!({"idle":true}), "idle"),
            ("agent_start", serde_json::json!({}), "working"),
            ("atm_needs_input_open", serde_json::json!({"title":reference}), "attention_needed"),
            ("atm_needs_input_resolved", serde_json::json!({"idle":false}), "working"),
            ("model_select", serde_json::json!({"model":{"id":"probe"}}), "working"),
            ("agent_settled", serde_json::json!({}), "idle"),
            ("atm_needs_input_open", serde_json::json!({"title":reference}), "attention_needed"),
            // Missing idle is also the forwarder's input-latched projection.
            ("atm_needs_input_resolved", serde_json::json!({}), "working"),
            ("agent_settled", serde_json::json!({}), "idle"),
            ("atm_needs_input_open", serde_json::json!({"title":reference}), "attention_needed"),
            ("atm_needs_input_resolved", serde_json::json!({"idle":null}), "working"),
            ("agent_settled", serde_json::json!({}), "idle"),
        ] {
            let data = serde_json::json!({"event":event,"payload":payload,"session_id":session.as_str(),
                "pid":pid,"tmux_pane":"%7","model":{"id":"probe","provider":"synthetic"}});
            cases.push((data, expected));
        }
        let adapter = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../atm-pi-adapter");
        if adapter.join("Cargo.toml").exists() {
            // Captured: pi 1.5.2, 2026-11-16, sealed PTY/private-atmd proxy,
            // Enter and Esc. Synthetic: attribution to this registry/pane.
            // Published atmd packages omit sibling adapter fixtures.
            let text = std::fs::read_to_string(adapter.join("tests/fixtures/pi_dialog_1.5.2.jsonl")).unwrap();
            let mut captured = 0;
            for line in text.lines() {
                let frame: serde_json::Value = serde_json::from_str(line).unwrap();
                let mut data = frame["data"].clone();
                let expected = match data["event"].as_str().unwrap() {
                    "atm_needs_input_open" => "attention_needed",
                    "atm_needs_input_resolved" => "idle",
                    _ => continue,
                };
                data["session_id"] = session.as_str().into();
                data["pid"] = pid.into();
                data["tmux_pane"] = "%7".into();
                cases.push((data, expected));
                captured += 1;
            }
            assert_eq!(captured, 4);
        }
        for (data, expected) in cases {
            let event = data["event"].as_str().unwrap().to_owned();
            let payload = data["payload"].clone();
            let raw: atm_pi_adapter::RawPiEvent = serde_json::from_value(data.clone()).unwrap();
            for lifecycle in raw.to_lifecycle_events() {
                assert!(atm_org::TaskUsageObservation::from_lifecycle(session.clone(), Some("%7".into()), &lifecycle).is_none());
            }
            assert!(raw.accounting_usage().is_none());
            send(&mut peer, ClientMessage::pi_event(data)).await;
            send(&mut peer, ClientMessage::list_sessions()).await;
            let response = recv(&mut peer).await;
            let rows = response["sessions"].as_array().unwrap();
            assert_eq!(rows.len(), 1);
            let row = rows.first().unwrap();
            assert_eq!(row["id"], session.as_str());
            assert_eq!(row["status"], expected, "{event} {payload}: {response}");
            if event == "atm_needs_input_open" { assert_eq!(row["activity_detail"], payload["title"]); }
            assert!(row["first_prompt"].is_null());
            assert_eq!(row["cost_usd"], 0.0);
            assert!(registry.pane_sessions("%7").await.unwrap().first().unwrap().start.is_none());
            assert!(evidence.receipt(message_id).unwrap().is_none(), "dialog title is not prompt evidence");
        }
        assert!(evidence.snapshot(message_id).unwrap().probably_landed, "ordinary proven activity still reaches evidence");
        cancel.cancel(); task.await.unwrap();
    }).await.unwrap();
}

fn pi_event_json(event: &str, payload: serde_json::Value) -> serde_json::Value {
    // No pid — the test session is registered with a synthetic PID;
    // the registry resolves via `session_id_to_pid`. (Production traffic
    // from the pi extension WILL include pid; tests covering that path
    // would also pass it explicitly.)
    serde_json::json!({
        "event": event,
        "payload": payload,
        "session_id": "e2e-pi",
    })
}

#[tokio::test]
async fn test_e2e_pi_tool_call_translates_to_tool_call_start() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-pi");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    // Pi tool_call (the canonical "tool starting" signal in pi).
    client
        .send(ClientMessage::pi_event(pi_event_json(
            "tool_call",
            serde_json::json!({
                "type": "tool_call",
                "toolName": "Bash",
                "toolCallId": "toolu_pi_demo",
                "input": {"command": "ls"}
            }),
        )))
        .await;
    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session should exist");
    assert_eq!(view.status_label, "working");
    assert_eq!(view.activity_detail, Some("Bash".into()));

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_pi_permission_gate_becomes_needs_input() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-pi");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    // tool_call with needs_user_input=true is the extension's signal
    // that pi has reached ctx.ui.select(...) and is awaiting the user.
    // This is the load-bearing "NeedsInput requires extension
    // participation" finding from the spike.
    client
        .send(ClientMessage::pi_event(pi_event_json(
            "tool_call",
            serde_json::json!({
                "type": "tool_call",
                "toolName": "Bash",
                "toolCallId": "toolu_dangerous",
                "needs_user_input": true
            }),
        )))
        .await;
    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session should exist");
    assert_eq!(view.status_label, "needs input");
    assert_eq!(view.activity_detail, Some("Bash".into()));

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_pi_agent_end_translates_to_idle() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-pi");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    // Drive Working then Idle.
    client
        .send(ClientMessage::pi_event(pi_event_json(
            "agent_start",
            serde_json::json!({"type":"agent_start"}),
        )))
        .await;
    client.applied(0).await;

    client
        .send(ClientMessage::pi_event(pi_event_json(
            "agent_end",
            serde_json::json!({"type":"agent_end"}),
        )))
        .await;
    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session should still exist");
    assert_eq!(view.status_label, "idle");

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_pi_context_event_drives_cost_and_token_display() {
    // Pi's `context` event carries cumulative usage on the latest
    // assistant message. The pi adapter extracts cost/tokens, the
    // daemon applies them to Session.cost / Session.context — same
    // fields the Claude status-line path populates. So the TUI shows
    // a non-zero $ and token count for pi sessions.
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-pi");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session exists");
    assert!((view.cost_usd - 0.0).abs() < 1e-9, "cost starts at $0");

    client
        .send(ClientMessage::pi_event(pi_event_json(
            "context",
            serde_json::json!({
                "type": "context",
                "messages": [
                    {"role": "user", "content": "hi"},
                    {
                        "role": "assistant",
                        "usage": {
                            "input": 1088,
                            "output": 55,
                            "totalTokens": 1143,
                            "cost": {"input": 0.00544, "output": 0.00165, "total": 0.00709}
                        }
                    }
                ]
            }),
        )))
        .await;
    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session exists");
    assert!(
        (view.cost_usd - 0.00709).abs() < 1e-9,
        "expected cost 0.00709, got {}",
        view.cost_usd
    );

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_pi_atm_needs_input_open_drives_attention_needed() {
    // Synthetic event the pi-atm extension emits when it
    // intercepts a `ctx.ui.select(...)` call (e.g. pi-amplike's
    // bash permission gate). This is the load-bearing finding from
    // the spike encoded as runtime instrumentation.
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-pi");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    client
        .send(ClientMessage::pi_event(pi_event_json(
            "atm_needs_input_open",
            serde_json::json!({"title": "Allow `rm -rf /tmp/junk`?"}),
        )))
        .await;
    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session exists");
    assert_eq!(view.status_label, "needs input");

    // The dialog closes — session resumes work.
    client
        .send(ClientMessage::pi_event(pi_event_json(
            "atm_needs_input_resolved",
            serde_json::json!({}),
        )))
        .await;
    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session exists");
    assert_eq!(view.status_label, "working");

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_pi_session_shutdown_removes_session() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-pi");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    client
        .send(ClientMessage::pi_event(pi_event_json(
            "session_shutdown",
            serde_json::json!({"type":"session_shutdown","reason":"quit"}),
        )))
        .await;
    client.applied(0).await;

    assert!(
        registry.get_session(session_id.clone()).await.is_none(),
        "session_shutdown should remove the session"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_pi_suppressed_event_does_not_panic_or_disrupt() {
    // tool_execution_start is suppressed by the adapter (returns None
    // from to_lifecycle_event). The connection layer should accept it
    // silently and leave session state unchanged.
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-pi");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    let before = registry
        .get_session(session_id.clone())
        .await
        .expect("exists");

    client
        .send(ClientMessage::pi_event(pi_event_json(
            "tool_execution_start",
            serde_json::json!({
                "type":"tool_execution_start",
                "toolName":"ls",
                "toolCallId":"call_x"
            }),
        )))
        .await;
    client.applied(0).await;

    let after = registry
        .get_session(session_id.clone())
        .await
        .expect("still exists");
    assert_eq!(before.status_label, after.status_label);

    server.shutdown().await;
}

/// The `data` of each frame in one of atm-pi-adapter's captured pi 0.73.1
/// fixtures, as pi-atm sent it to atmd.
///
/// Each frame's pid becomes this test's own pid. The captured pi has
/// exited, and the registry's cleanup sweep drops sessions whose pid is
/// dead.
///
/// `None` outside the workspace: atmd's tests ship in its crates.io
/// package without the adapter's fixtures, so the caller skips.
fn captured_pi_events(fixture: &str) -> Option<Vec<serde_json::Value>> {
    let adapter = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../atm-pi-adapter");
    if !adapter.join("Cargo.toml").exists() {
        eprintln!("skipped: no {} outside the workspace", adapter.display());
        return None;
    }
    let path = adapter.join("tests/fixtures").join(fixture);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let events = text
        .lines()
        .map(|line| {
            let frame: serde_json::Value = serde_json::from_str(line).expect("fixture line");
            let mut data = frame["data"].clone();
            data["pid"] = std::process::id().into();
            data
        })
        .collect();
    Some(events)
}

/// Read one snapshot after the caller's same-stream apply fence, retaining
/// the exact one-real-session assertions instead of polling snapshots.
async fn only_session(registry: &atmd::registry::RegistryHandle) -> SessionView {
    let mut sessions = registry.get_all_sessions().await;
    assert_eq!(sessions.len(), 1, "expected one session, got {sessions:?}");
    let session = sessions.remove(0);
    assert!(
        !session.id.is_pending(),
        "pi-atm's session_id should replace {}",
        session.id
    );
    session
}

/// Assert one post-fence snapshot holds no sessions.
async fn assert_no_sessions(registry: &atmd::registry::RegistryHandle) {
    let sessions = registry.get_all_sessions().await;
    assert!(
        sessions.is_empty(),
        "expected no sessions, got {sessions:?}"
    );
}

#[tokio::test]
async fn test_e2e_pi_captured_run_upgrades_pending_session() {
    // Discovery registers an agent it recognizes as `pending-<pid>`.
    // pi-atm's first frame names the session, and atmd renames that row
    // rather than adding a second one. (A pi that starts after atmd is not
    // discovered first: its row starts at that first frame, as in the
    // new-session test below.)
    let Some(events) = captured_pi_events("pi_tool_run_0.73.1.jsonl") else {
        return;
    };
    let (last, rest) = events.split_last().expect("captured frames");
    assert_eq!(last["event"], "session_shutdown");

    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let pid = std::process::id();
    let pending = SessionId::pending_from_pid(pid);
    registry
        .register_discovered(
            pending.clone(),
            pid,
            std::env::temp_dir(),
            None,
            Harness::Pi,
        )
        .await
        .expect("register discovered session");

    for event in rest {
        client.send(ClientMessage::pi_event(event.clone())).await;
    }
    client.applied(0).await;

    let session = only_session(&registry).await;
    assert_eq!(Some(session.id.as_str()), rest[0]["session_id"].as_str());
    assert_eq!(session.harness, "pi");
    assert!(registry.get_session(pending).await.is_none());

    // The context window comes from pi-atm's model snapshot (fake-b's
    // 32000), not the unknown model's 200K default. The same-stream fence
    // covers the final context frame, not just its initial `0.0% (0/32K)`.
    let expected = "3.2% (1.0K/32K)";
    let sessions = registry.get_all_sessions().await;
    let displays: Vec<&str> = sessions
        .iter()
        .map(|s| s.context_display.as_str())
        .collect();
    assert_eq!(displays, [expected]);

    send_applied(&mut client, 1, ClientMessage::pi_event(last.clone())).await;
    assert_no_sessions(&registry).await;

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_pi_captured_new_session_replaces_session() {
    // `/new` in one pi 0.73.1 process: the old pi-atm instance ends its
    // session, then pi's new instance starts the new one on a second
    // connection. The pid stays; atmd shows one session at a time, under
    // pi's id.
    let Some(events) = captured_pi_events("pi_new_session_0.73.1.jsonl") else {
        return;
    };
    let [start, end_old, start_new, end] = events.as_slice() else {
        panic!("expected 4 frames, got {}", events.len());
    };

    let (server, registry) = TestServer::spawn_with_registry().await;

    let mut old = server.connect().await;
    old.handshake(None).await;
    send_applied(&mut old, 1, ClientMessage::pi_event(start.clone())).await;
    let old_id = only_session(&registry).await.id;
    assert_eq!(Some(old_id.as_str()), start["session_id"].as_str());

    send_applied(&mut old, 2, ClientMessage::pi_event(end_old.clone())).await;
    assert_no_sessions(&registry).await;

    let mut new = server.connect().await;
    new.handshake(None).await;
    send_applied(&mut new, 1, ClientMessage::pi_event(start_new.clone())).await;
    let new_id = only_session(&registry).await.id;
    assert_eq!(Some(new_id.as_str()), start_new["session_id"].as_str());
    assert_ne!(new_id, old_id);

    send_applied(&mut new, 2, ClientMessage::pi_event(end.clone())).await;
    assert_no_sessions(&registry).await;

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_pi_captured_session_cost_is_cumulative() {
    // Two prompts and three priced requests in one pi 0.73.1 session. Its
    // cost is the sum, $0.01065, as pi's footer showed (`$0.011`), not the
    // last request's $0.003675.
    let Some(events) = captured_pi_events("pi_session_cost_0.73.1.jsonl") else {
        return;
    };
    let (last, rest) = events.split_last().expect("captured frames");
    assert_eq!(last["event"], "session_shutdown");

    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut updates = registry.subscribe();
    let mut client = server.connect().await;
    client.handshake(None).await;
    timeout(Duration::from_secs(60), async {
        for event in rest {
            client.send(ClientMessage::pi_event(event.clone())).await;
        }
        // The same connection handles frames in order: Pong is a barrier
        // after every lifecycle and derived metadata apply, not a delay.
        client.send(ClientMessage::ping(1)).await;
        assert!(matches!(
            client.recv().await,
            DaemonMessage::Pong { seq: 1, .. }
        ));
    })
    .await
    .expect("captured pi frames applied");

    let sessions = registry.get_all_sessions().await;
    let shown: Vec<(&str, f64, &str)> = sessions
        .iter()
        .map(|s| (s.model.as_str(), s.cost_usd, s.context_display.as_str()))
        .collect();
    assert_eq!(shown, [("fake-b", 0.01065, "3.8% (1.2K/32K)")]);
    let mut idle_costs = Vec::new();
    while let Ok(event) = updates.try_recv() {
        if let atmd::registry::SessionEvent::Updated { session } = event {
            if session.status == atm_core::SessionStatus::Idle && session.cost_usd > 0.0 {
                assert_eq!(session.model, "fake-b");
                if idle_costs.last() != Some(&session.cost_usd) {
                    idle_costs.push(session.cost_usd);
                }
            }
        }
    }
    assert_eq!(idle_costs, [0.006975, 0.01065]);

    server.cancel_token.cancel();
    timeout(Duration::from_secs(60), server.run_task)
        .await
        .expect("server stops")
        .unwrap();
}

// ============================================================================
// Codex Event → LifecycleEvent translation (end-to-end)
//
// Symmetric with the Claude and pi blocks above: drives Codex raw
// hook JSON (shapes captured from codex-cli 0.146.1 during the
// 2026-08-10 spike) over the wire via `ClientMessage::codex_event` and
// asserts on the resulting session state.
//
// Synthetic: payloads hand-built with `codex_event_json` in the spike's
// shapes. The real codex-cli 0.163.0 capture is atm-codex-adapter's
// tests/fixtures/codex_tui_session_0.163.0.jsonl.
// ============================================================================

/// Builds a raw Codex hook-event JSON payload for the given session.
fn codex_event_json(
    session_id: &str,
    event_name: &str,
    extras: serde_json::Value,
) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "session_id": session_id,
        "hook_event_name": event_name,
    });
    if let (Some(map), Some(extras)) = (obj.as_object_mut(), extras.as_object()) {
        for (k, v) in extras {
            map.insert(k.clone(), v.clone());
        }
    }
    obj
}

#[tokio::test]
async fn test_e2e_codex_session_start_applies_model() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-codex-model");
    client
        .send(ClientMessage::codex_event(codex_event_json(
            session_id.as_str(),
            "SessionStart",
            serde_json::json!({
                "source": "startup",
                "model": "gpt-5.6-sol",
                "pid": std::process::id()
            }),
        )))
        .await;

    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("SessionStart should create the session");
    assert_eq!(view.model, "gpt-5.6-sol");

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_codex_event_applies_transcript_token_usage() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    // Synthetic: a hand-written token_count record; the origin of its
    // numbers was not recorded (added in 49f2350 on 2026-08-16).
    let transcript_dir = tempfile::tempdir().expect("create transcript directory");
    let transcript_path = transcript_dir.path().join("rollout.jsonl");
    std::fs::write(
        &transcript_path,
        concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{",
            "\"total_token_usage\":{\"total_tokens\":94375},",
            "\"last_token_usage\":{\"total_tokens\":25298},",
            "\"model_context_window\":258400}}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n"
        ),
    )
    .expect("write transcript");

    let session_id = SessionId::new("e2e-codex-context");
    client
        .send(ClientMessage::codex_event(codex_event_json(
            session_id.as_str(),
            "SessionStart",
            serde_json::json!({
                "source": "startup",
                "model": "gpt-5.6-sol",
                "pid": std::process::id(),
                "transcript_path": transcript_path
            }),
        )))
        .await;

    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("SessionStart should create the session");
    assert!((view.context_percentage - 9.790_247).abs() < 0.001);
    assert_eq!(view.context_display, "9.8% (25K/258K)");

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_codex_pre_tool_use_translates_to_tool_call_start() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-codex-tool");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    client
        .send(ClientMessage::codex_event(codex_event_json(
            session_id.as_str(),
            "PreToolUse",
            serde_json::json!({
                "tool_name": "Bash",
                "tool_input": {"command": "echo hi"},
                "tool_use_id": "exec-0faab0a6"
            }),
        )))
        .await;

    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session should still exist");
    assert_eq!(
        view.status_label, "working",
        "PreToolUse(Bash) should translate to ToolCallStart -> Working"
    );
    assert_eq!(view.activity_detail, Some("Bash".into()));

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_codex_permission_request_becomes_needs_input() {
    // The load-bearing Codex-specific case: a vendor-signaled
    // permission gate (not inferred from an interactive-tool
    // allowlist) must flip the session to needs-input.
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-codex-perm");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    // Spike-verified ordering: PreToolUse fires first for the same
    // gated call, then PermissionRequest.
    client
        .send(ClientMessage::codex_event(codex_event_json(
            session_id.as_str(),
            "PreToolUse",
            serde_json::json!({"tool_name": "Bash"}),
        )))
        .await;
    client
        .send(ClientMessage::codex_event(codex_event_json(
            session_id.as_str(),
            "PermissionRequest",
            serde_json::json!({
                "tool_name": "Bash",
                "tool_input": {"command": "touch x"}
            }),
        )))
        .await;

    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session should still exist");
    assert_eq!(
        view.status_label, "needs input",
        "PermissionRequest should translate to NeedsInput(PermissionGate) -> AttentionNeeded"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_codex_stop_translates_to_idle() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-codex-stop");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    client
        .send(ClientMessage::codex_event(codex_event_json(
            session_id.as_str(),
            "PreToolUse",
            serde_json::json!({"tool_name": "Bash"}),
        )))
        .await;
    client.applied(0).await;

    client
        .send(ClientMessage::codex_event(codex_event_json(
            session_id.as_str(),
            "Stop",
            serde_json::json!({"stop_hook_active": false, "last_assistant_message": "done"}),
        )))
        .await;
    client.applied(0).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("session should still exist after Stop");
    assert_eq!(
        view.status_label, "idle",
        "Stop should translate to WorkingEnd -> Idle"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_codex_session_end_removes_session() {
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-codex-end");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    assert!(registry.get_session(session_id.clone()).await.is_some());

    client
        .send(ClientMessage::codex_event(codex_event_json(
            session_id.as_str(),
            "SessionEnd",
            serde_json::json!({"reason": "other"}),
        )))
        .await;
    client.applied(0).await;

    assert!(
        registry.get_session(session_id.clone()).await.is_none(),
        "SessionEnd should remove the session from the registry"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_codex_malformed_tool_event_is_suppressed() {
    // PreToolUse without a tool_name is dropped by the adapter guard.
    // The connection layer should accept it silently and leave session
    // state unchanged.
    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    let session_id = SessionId::new("e2e-codex-malformed");
    registry
        .register(create_test_session(session_id.as_str()))
        .await
        .expect("register session");

    let before = registry
        .get_session(session_id.clone())
        .await
        .expect("exists");

    client
        .send(ClientMessage::codex_event(codex_event_json(
            session_id.as_str(),
            "PreToolUse",
            serde_json::json!({}),
        )))
        .await;
    client.applied(0).await;

    let after = registry
        .get_session(session_id.clone())
        .await
        .expect("still exists");
    assert_eq!(before.status_label, after.status_label);

    server.shutdown().await;
}

/// The hook payloads in one of atm-codex-adapter's captured codex-cli
/// 0.163.0 fixtures, as codex wrote them to the hook's stdin, each given this
/// test's pid as atm-codex-hook would add one.
///
/// `None` outside the workspace: atmd's tests ship in its crates.io
/// package without the adapter's fixtures, so the caller skips.
fn captured_codex_events(fixture: &str) -> Option<Vec<serde_json::Value>> {
    captured_hook_payloads("atm-codex-adapter", fixture)
}

/// The hook payloads in `fixture`, one per line in the `adapter` crate's
/// `tests/fixtures`, each given this test's pid as the hook script would
/// add one. `None` outside the workspace, as for [`captured_codex_events`].
fn captured_hook_payloads(adapter: &str, fixture: &str) -> Option<Vec<serde_json::Value>> {
    let adapter = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(adapter);
    if !adapter.join("Cargo.toml").exists() {
        eprintln!("skipped: no {} outside the workspace", adapter.display());
        return None;
    }
    let path = adapter.join("tests/fixtures").join(fixture);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let events = text
        .lines()
        .map(|line| {
            let mut payload: serde_json::Value = serde_json::from_str(line).expect("fixture line");
            payload["pid"] = std::process::id().into();
            payload
        })
        .collect();
    Some(events)
}

/// Applies one codex hook payload (see [`send_applied`]) and returns the
/// sessions.
async fn apply_codex_event(
    client: &mut TestClient,
    registry: &atmd::registry::RegistryHandle,
    seq: u64,
    event: &serde_json::Value,
) -> Vec<SessionView> {
    send_applied(client, seq, ClientMessage::codex_event(event.clone())).await;
    registry.get_all_sessions().await
}

/// (status label, activity) of the one codex session after a payload.
type CodexState = (&'static str, Option<&'static str>);

/// Applies each payload in order and asserts the one session's state
/// after it. Each step is labelled by its payload's hook event.
async fn assert_codex_replay(
    client: &mut TestClient,
    registry: &atmd::registry::RegistryHandle,
    steps: &[(&serde_json::Value, CodexState)],
    fixture: &str,
) {
    let state = |s: &SessionView| (s.status_label.clone(), s.activity_detail.clone());
    let mut seen = Vec::new();
    let mut expected = Vec::new();
    for (seq, (event, (label, detail))) in (1..).zip(steps) {
        let sessions = apply_codex_event(client, registry, seq, event).await;
        let name = event["hook_event_name"].as_str().unwrap_or_default();
        seen.push((name, sessions.iter().map(state).collect::<Vec<_>>()));
        let want = vec![(label.to_string(), detail.map(String::from))];
        expected.push((name, want));
    }
    assert_eq!(seen, expected, "{fixture}");
}

#[tokio::test]
async fn test_e2e_codex_captured_session_replay() {
    // Captured: codex-cli 0.163.0, 2026-10-24, one TUI session: a prompt,
    // an approved tool call, `/compact`, then SessionEnd when codex's
    // shared daemon stopped (see atm-codex-adapter's crate docs).
    let fixture = "codex_tui_session_0.163.0.jsonl";
    let Some(events) = captured_codex_events(fixture) else {
        return;
    };
    let [start, prompt, tool_start, permission, tool_end, stop, pre_compact, post_compact, end] =
        events.as_slice()
    else {
        panic!("expected 9 hook payloads, got {}", events.len());
    };

    let (server, registry) = TestServer::spawn_with_registry().await;
    let mut client = server.connect().await;
    client.handshake(None).await;
    let steps = [
        (start, ("idle", None)),
        (prompt, ("working", None)),
        (tool_start, ("working", Some("Bash"))),
        (permission, ("needs input", Some("Bash"))),
        (tool_end, ("working", Some("Thinking"))),
        (stop, ("idle", None)),
        (pre_compact, ("working", Some("Compacting"))),
        // No Stop follows a manual /compact, so PostCompact ends it.
        (post_compact, ("idle", None)),
    ];
    assert_codex_replay(&mut client, &registry, &steps, fixture).await;
    let models: Vec<String> = registry
        .get_all_sessions()
        .await
        .iter()
        .map(|s| s.model.clone())
        .collect();
    assert_eq!(models, ["fake-model"]);

    let sessions = apply_codex_event(&mut client, &registry, 0, end).await;
    assert!(
        sessions.is_empty(),
        "SessionEnd removes the row: {sessions:?}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn test_e2e_codex_captured_auto_compact_replay() {
    // Captured: codex-cli 0.163.0, 2026-10-24, auto compactions in the
    // TUI (mid-turn, then before the next turn) and in `codex exec`
    // (after the turn's Stop); see atm-codex-adapter's crate docs.
    const IDLE: CodexState = ("idle", None);
    const WORKING: CodexState = ("working", None);
    const BASH: CodexState = ("working", Some("Bash"));
    const THINKING: CodexState = ("working", Some("Thinking"));
    const COMPACTING: CodexState = ("working", Some("Compacting"));
    // One state per payload but the final SessionEnd.
    let tui = [
        IDLE,       // SessionStart startup
        WORKING,    // UserPromptSubmit
        BASH,       // PreToolUse
        THINKING,   // PostToolUse
        COMPACTING, // PreCompact auto, mid-turn
        // Known gap (crate docs): the turn goes on to its Stop, but
        // PostCompact and then SessionStart{compact} show the row idle.
        IDLE,       // PostCompact auto
        IDLE,       // SessionStart compact
        IDLE,       // Stop
        COMPACTING, // PreCompact auto: the next prompt compacts first
        IDLE,       // PostCompact auto
        IDLE,       // SessionStart compact
        WORKING,    // UserPromptSubmit
        BASH,       // PreToolUse
        THINKING,   // PostToolUse
        COMPACTING, // PreCompact auto, mid-turn
        IDLE,       // PostCompact auto: the same known gap
        IDLE,       // SessionStart compact
        IDLE,       // Stop
    ];
    let exec = [
        IDLE,       // SessionStart startup
        WORKING,    // UserPromptSubmit
        BASH,       // PreToolUse
        THINKING,   // PostToolUse
        IDLE,       // Stop
        COMPACTING, // PreCompact auto, after Stop
        IDLE,       // PostCompact auto: nothing follows it
    ];
    let cases: [(&str, &[CodexState]); 2] = [
        ("codex_tui_auto_compact_0.163.0.jsonl", &tui),
        ("codex_exec_post_turn_compact_0.163.0.jsonl", &exec),
    ];
    for (fixture, states) in cases {
        let Some(events) = captured_codex_events(fixture) else {
            return;
        };
        let Some((end, events)) = events.split_last() else {
            panic!("{fixture} is empty");
        };
        assert_eq!(events.len(), states.len(), "{fixture}");

        let (server, registry) = TestServer::spawn_with_registry().await;
        let mut client = server.connect().await;
        client.handshake(None).await;
        let steps: Vec<_> = events.iter().zip(states.iter().copied()).collect();
        assert_codex_replay(&mut client, &registry, &steps, fixture).await;

        let sessions = apply_codex_event(&mut client, &registry, 0, end).await;
        assert!(sessions.is_empty(), "{fixture}: {sessions:?}");
        server.shutdown().await;
    }
}

#[tokio::test]
async fn test_concurrent_ping_pong() {
    let server = TestServer::spawn().await;

    // Create 3 clients
    let mut clients = Vec::new();
    for i in 0..3 {
        let mut client = server.connect().await;
        client.handshake(Some(format!("ping-client-{i}"))).await;
        clients.push(client);
    }

    // Send pings concurrently with different seq numbers
    for (i, client) in clients.iter_mut().enumerate() {
        client.send(ClientMessage::ping((i * 100) as u64)).await;
    }

    // Receive pongs and verify correct seq
    for (i, client) in clients.iter_mut().enumerate() {
        match client.recv().await {
            DaemonMessage::Pong { seq } => {
                assert_eq!(seq, (i * 100) as u64);
            }
            other => panic!("Expected Pong for client {i}, got {other:?}"),
        }
    }

    server.shutdown().await;
}
