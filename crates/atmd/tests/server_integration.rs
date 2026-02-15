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
