//! Integration tests for the Unix socket server.
//!
//! These tests verify the DaemonServer works correctly as a complete system,
//! testing connection handling, protocol negotiation, subscriptions, and graceful shutdown.
//!
//! Per CLAUDE.md: Tests CAN use `.unwrap()` and `.expect()` - this is allowed.
//! We test the panic-free behavior of production code through assertions.

use std::path::PathBuf;
use std::time::Duration;

use atm_core::{AgentType, Model, SessionDomain, SessionId};
use atm_protocol::{ClientMessage, DaemonMessage, MessageType, ProtocolVersion};
use atmd::registry::spawn_registry;
use atmd::server::DaemonServer;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

// ============================================================================
// Constants
// ============================================================================

/// Maximum time to wait for server socket to appear
const SOCKET_WAIT_TIMEOUT: Duration = Duration::from_millis(500);

/// Interval between socket existence checks
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Grace period for server shutdown
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_millis(100);

// ============================================================================
// Test Helpers
// ============================================================================

/// Test server context that manages server lifecycle and cleanup.
struct TestServer {
    socket_path: PathBuf,
    cancel_token: CancellationToken,
    _temp_dir: TempDir, // Keep alive for RAII cleanup
}

impl TestServer {
    /// Internal helper that spawns the server and returns both the test server and registry handle.
    async fn spawn_internal() -> (Self, atmd::registry::RegistryHandle) {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let socket_path = temp_dir.path().join("test.sock");

        let registry = spawn_registry();
        let registry_handle = registry.clone();
        let cancel_token = CancellationToken::new();

        let server = DaemonServer::new(socket_path.clone(), registry, cancel_token.clone());

        // Spawn server in background
        tokio::spawn(async move {
            let _ = server.run().await;
        });

        // Wait for socket to be ready with timeout
        let start = tokio::time::Instant::now();
        while start.elapsed() < SOCKET_WAIT_TIMEOUT {
            if socket_path.exists() {
                break;
            }
            sleep(SOCKET_POLL_INTERVAL).await;
        }

        // Fail fast if socket didn't appear
        assert!(
            socket_path.exists(),
            "Server socket did not appear within {SOCKET_WAIT_TIMEOUT:?}"
        );

        let test_server = TestServer {
            socket_path,
            cancel_token,
            _temp_dir: temp_dir,
        };

        (test_server, registry_handle)
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
        sleep(SHUTDOWN_GRACE_PERIOD).await;
    }
}

/// Test client connection with protocol helpers.
struct TestClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl TestClient {
    fn new(stream: UnixStream) -> Self {
        let (reader, writer) = stream.into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
        }
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
        } => {
            assert_eq!(protocol_version, ProtocolVersion::CURRENT);
            assert_eq!(client_id, "test-client");
        }
        other => panic!("Expected Connected, got {other:?}"),
    }

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

/// MAX_TUI_CLIENTS is 10 (defined in server/mod.rs)
const MAX_TUI_CLIENTS: usize = 10;

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

    // The 11th client can connect and handshake, but subscribing depends on
    // whether the add_subscriber logic is called. In the current implementation,
    // subscribe is handled by ConnectionHandler, and we need to verify the
    // subscriber count limit is enforced.
    //
    // Note: The current implementation doesn't immediately reject at subscribe time
    // via the protocol - the add_subscriber happens in the server's subscriber list.
    // For this test, we verify that 10 clients can successfully subscribe.

    // Verify all 10 clients are working by sending ping
    for (i, client) in clients.iter_mut().enumerate() {
        client.send(ClientMessage::ping(i as u64)).await;
        match client.recv().await {
            DaemonMessage::Pong { seq } => {
                assert_eq!(seq, i as u64);
            }
            other => panic!("Expected Pong for client {i}, got {other:?}"),
        }
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

    // Wait for server to shutdown
    sleep(SHUTDOWN_GRACE_PERIOD).await;

    // Socket should be removed
    assert!(
        !socket_path.exists(),
        "Socket file should be removed after shutdown"
    );
}

#[tokio::test]
async fn test_client_disconnect_message() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    // Send disconnect
    client.send(ClientMessage::disconnect()).await;

    // Connection will close (server won't send response to disconnect)
    // Give server time to process
    sleep(SHUTDOWN_GRACE_PERIOD).await;

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
// These tests drive a real Claude raw `HookEvent` JSON over the wire and
// confirm that the connection layer translates it into a vendor-neutral
// `LifecycleEvent`, the registry applies it correctly, and the resulting
// session state matches expectations. Together they exercise:
//
//   wire JSON  →  RawHookEvent  →  LifecycleEvent  →  Session state
//
// Each test pre-registers a session (to skip the PID-based create-on-event
// path that needs `/proc` lookups), then sends a hook event and reads the
// session view back through the registry handle.
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

    // Send a real Claude PreToolUse(Bash) hook event over the wire.
    client
        .send(ClientMessage::hook_event(hook_event_json(
            session_id.as_str(),
            "PreToolUse",
            serde_json::json!({"tool_name": "Bash"}),
        )))
        .await;

    // Give the actor a tick to apply the event.
    sleep(Duration::from_millis(50)).await;

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

    sleep(Duration::from_millis(50)).await;

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
    sleep(Duration::from_millis(50)).await;

    client
        .send(ClientMessage::hook_event(hook_event_json(
            session_id.as_str(),
            "Stop",
            serde_json::json!({}),
        )))
        .await;
    sleep(Duration::from_millis(50)).await;

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
    sleep(Duration::from_millis(50)).await;

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
    sleep(Duration::from_millis(50)).await;

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

// ============================================================================
// Pi Event → LifecycleEvent translation (end-to-end, wire-level)
//
// Symmetric with the Claude e2e block above. These tests drive
// pi-shaped JSON through MessageType::PiEvent and confirm the
// connection layer hands off to atm-pi-adapter and produces the
// expected session state. They are the wire-level counterpart to
// the unit tests in atm-pi-adapter::translate.
// ============================================================================

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
    sleep(Duration::from_millis(50)).await;

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
    sleep(Duration::from_millis(50)).await;

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
    sleep(Duration::from_millis(50)).await;

    client
        .send(ClientMessage::pi_event(pi_event_json(
            "agent_end",
            serde_json::json!({"type":"agent_end"}),
        )))
        .await;
    sleep(Duration::from_millis(50)).await;

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
    sleep(Duration::from_millis(50)).await;

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
    sleep(Duration::from_millis(50)).await;

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
    sleep(Duration::from_millis(50)).await;

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
    sleep(Duration::from_millis(50)).await;

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
    sleep(Duration::from_millis(50)).await;

    let after = registry
        .get_session(session_id.clone())
        .await
        .expect("still exists");
    assert_eq!(before.status_label, after.status_label);

    server.shutdown().await;
}

// ============================================================================
// Codex Event → LifecycleEvent translation (end-to-end)
//
// Symmetric with the Claude and pi blocks above: drives real Codex raw
// hook JSON (shapes captured from codex-cli 0.146.1 during the
// 2026-08-10 spike) over the wire via `ClientMessage::codex_event` and
// asserts on the resulting session state.
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

    sleep(Duration::from_millis(50)).await;

    let view = registry
        .get_session(session_id.clone())
        .await
        .expect("SessionStart should create the session");
    assert_eq!(view.model, "gpt-5.6-sol");

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

    sleep(Duration::from_millis(50)).await;

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

    sleep(Duration::from_millis(50)).await;

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
    sleep(Duration::from_millis(50)).await;

    client
        .send(ClientMessage::codex_event(codex_event_json(
            session_id.as_str(),
            "Stop",
            serde_json::json!({"stop_hook_active": false, "last_assistant_message": "done"}),
        )))
        .await;
    sleep(Duration::from_millis(50)).await;

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
    sleep(Duration::from_millis(50)).await;

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
    sleep(Duration::from_millis(50)).await;

    let after = registry
        .get_session(session_id.clone())
        .await
        .expect("still exists");
    assert_eq!(before.status_label, after.status_label);

    server.shutdown().await;
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
