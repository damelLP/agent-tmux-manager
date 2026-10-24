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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{sleep, timeout};
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
    run_task: tokio::task::JoinHandle<()>,
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
        let run_task = tokio::spawn(async move {
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
            run_task,
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

    // Wait for server to shutdown
    sleep(SHUTDOWN_GRACE_PERIOD).await;

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
/// and carrying on. Time is paused, so the timeout elapses as soon as the
/// pong writes block.
#[tokio::test(start_paused = true)]
async fn test_connection_closes_after_a_write_timeout() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;
    client.handshake(None).await;

    // Far more pongs than the socket buffer holds, sent without reading.
    // Writes fail once the server closes, so errors end the loop. The
    // write half stays open afterwards: closing it would send EOF and let
    // the server close for that reason instead.
    let mut writer = client.writer;
    tokio::spawn(async move {
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
    sleep(Duration::from_secs(30)).await;

    let drained = timeout(Duration::from_secs(60), async {
        let mut line = String::new();
        loop {
            line.clear();
            if client.reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
        }
    })
    .await;
    assert!(
        drained.is_ok(),
        "connection stayed open after a write timeout"
    );

    server.shutdown().await;
}

/// The error reply to a bad message can hit WRITE_TIMEOUT too, and may also
/// leave half a line, so it closes the connection like any failed reply.
/// The bad messages lack a session_id, so each one fails in the handler
/// and gets an error reply. Time is paused, as above.
#[tokio::test(start_paused = true)]
async fn test_connection_closes_when_an_error_reply_times_out() {
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
    tokio::spawn(async move {
        for _ in 0..20_000 {
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
        std::future::pending::<()>().await;
    });
    sleep(Duration::from_secs(30)).await;

    let drained = timeout(Duration::from_secs(60), async {
        let mut line = String::new();
        loop {
            line.clear();
            if client.reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
        }
    })
    .await;
    assert!(
        drained.is_ok(),
        "connection stayed open after an error reply timed out"
    );

    server.shutdown().await;
}

/// A client that connects but never sends `Connect` is dropped once the
/// handshake deadline passes. Time is paused, so the deadline elapses
/// instantly once the runtime is otherwise idle.
#[tokio::test(start_paused = true)]
async fn test_handshake_deadline_closes_silent_client() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

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
/// passes instantly while the server waits on the socket.
#[tokio::test(start_paused = true)]
async fn test_subscribed_client_stays_connected_past_read_timeout() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;
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
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;
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
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;
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

/// A tmux-mode teammate fires `TeammateIdle` from its *own* session. It
/// must idle that session, not spawn a `waiter@<teammate>` child under
/// it. Captured: Claude Code 2.1.288, 2026-10-04, a tmux-mode team run
/// (see atm-claude-adapter's crate docs); paths rewritten to
/// `/home/user` and `/tmp/claude-user`.
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
    sleep(Duration::from_millis(50)).await;
    let working = registry.get_session(teammate.clone()).await;
    assert_eq!(
        working.map(|view| view.status_label),
        Some("working".into())
    );

    let payload = serde_json::json!({
        "session_id": "34d56705-0907-49a6-8785-e72091a93178",
        "transcript_path": "/home/user/.claude/projects/-home-user-project/34d56705-0907-49a6-8785-e72091a93178.jsonl",
        "cwd": "/home/user/project",
        "scratchpad_dir": "/tmp/claude-user/-home-user-project/34d56705-0907-49a6-8785-e72091a93178/scratchpad",
        "permission_mode": "auto",
        "agent_type": "general-purpose",
        "hook_event_name": "TeammateIdle",
        "teammate_name": "waiter",
        "team_name": "session-ae223ce3"
    });
    client.send(ClientMessage::hook_event(payload)).await;
    sleep(Duration::from_millis(50)).await;

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
/// `/tmp/claude-user`. The payloads are those of atm-claude-adapter's
/// `claude_tui_session_2.1.310.jsonl` and atm-protocol's
/// `claude_status_line_2.1.310.jsonl`; this file adds their order.
const CLAUDE_INTERLEAVED: &str = "tests/fixtures/claude_tui_interleaved_2.1.310.jsonl";

/// The `(kind, payload)` of each call in [`CLAUDE_INTERLEAVED`], each
/// payload given this test's pid as atm-hook would add one.
fn captured_claude_calls() -> Vec<(String, serde_json::Value)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CLAUDE_INTERLEAVED);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    text.lines()
        .map(|line| {
            let mut call: serde_json::Value = serde_json::from_str(line).expect("fixture line");
            let kind = call["kind"].as_str().expect("kind").to_string();
            let mut payload = call["payload"].take();
            payload["pid"] = std::process::id().into();
            (kind, payload)
        })
        .collect()
}

#[test]
fn test_claude_interleaved_fixture_matches_its_sources() {
    // Every payload is byte-identical to its line in the two source
    // fixtures, so the interleaved fixture adds only the capture order.
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let hooks_path =
        crates.join("atm-claude-adapter/tests/fixtures/claude_tui_session_2.1.310.jsonl");
    let status_path = crates.join("atm-protocol/tests/fixtures/claude_status_line_2.1.310.jsonl");
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
async fn test_e2e_claude_captured_status_lines_leave_idle_rows_idle() {
    // Claude 2.1.310 sends a status line ~300 ms after every Stop, and
    // after `/compact`, `/clear` and a permission-mode change at the idle
    // prompt. Status lines carry no turn state, so none of them may turn
    // an idle row back to working.
    let calls = captured_claude_calls();
    assert_eq!(calls.len(), 46);
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
    let mut seen = Vec::new();
    for (seq, (i, (kind, payload))) in (1..).zip(calls.iter().enumerate()) {
        let message = match kind.as_str() {
            "hook" => ClientMessage::hook_event(payload.clone()),
            "status_line" => ClientMessage::status_update(payload.clone()),
            other => panic!("unexpected kind {other}"),
        };
        client.send(message).await;
        // atmd handles one connection's messages in order and awaits the
        // registry for each, so the Pong means the call has been applied.
        client.send(ClientMessage::ping(seq)).await;
        match client.recv().await {
            DaemonMessage::Pong { seq: got } if got == seq => {}
            other => panic!("expected Pong {seq}, got {other:?}"),
        }
        if let Some(&(_, before)) = checkpoints.iter().find(|(at, _)| *at == i) {
            let previous = calls[i - 1].1["hook_event_name"].as_str();
            let id = payload["session_id"].as_str().expect("session_id");
            let status = registry
                .get_session(SessionId::new(id))
                .await
                .map(|view| view.status_label);
            seen.push((i, kind.as_str(), previous, status));
            assert_eq!(previous, Some(before), "fixture order at {i}");
        }
    }
    let expected: Vec<_> = checkpoints
        .iter()
        .map(|&(i, before)| (i, "status_line", Some(before), Some("idle".to_string())))
        .collect();
    assert_eq!(seen, expected);

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
//
// Synthetic: payloads hand-built with `pi_event_json`, except the
// `test_e2e_pi_captured_*` tests, which replay atm-pi-adapter's real pi
// 0.73.1 captures (tests/fixtures/*_0.73.1.jsonl) over the socket.
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

/// Polls the registry until `done` holds for its sessions, for up to 2 s,
/// and returns the last snapshot.
async fn sessions_when(
    registry: &atmd::registry::RegistryHandle,
    done: impl Fn(&[SessionView]) -> bool,
) -> Vec<SessionView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let sessions = registry.get_all_sessions().await;
        if done(&sessions) || tokio::time::Instant::now() >= deadline {
            return sessions;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

/// Waits for the registry to hold one session with a real (not pending)
/// id, and returns it.
async fn only_session(registry: &atmd::registry::RegistryHandle) -> SessionView {
    let mut sessions =
        sessions_when(registry, |s| matches!(s, [one] if !one.id.is_pending())).await;
    assert_eq!(sessions.len(), 1, "expected one session, got {sessions:?}");
    let session = sessions.remove(0);
    assert!(
        !session.id.is_pending(),
        "pi-atm's session_id should replace {}",
        session.id
    );
    session
}

/// Waits for the registry to hold no sessions.
async fn assert_no_sessions(registry: &atmd::registry::RegistryHandle) {
    let sessions = sessions_when(registry, <[SessionView]>::is_empty).await;
    assert!(
        sessions.is_empty(),
        "expected no sessions, got {sessions:?}"
    );
}

#[tokio::test]
async fn test_e2e_pi_captured_run_upgrades_pending_session() {
    // Discovery registers an agent it recognizes as `pending-<pid>`.
    // pi-atm's first frame names the session, and atmd renames that row
    // rather than adding a second one. (Discovery can't see pi 0.73.1, see
    // atm-pi-adapter's Known gaps, so today pi's row starts at that first
    // frame, as in the new-session test below.)
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

    let session = only_session(&registry).await;
    assert_eq!(Some(session.id.as_str()), rest[0]["session_id"].as_str());
    assert_eq!(session.harness, "pi");
    assert!(registry.get_session(pending).await.is_none());

    // The context window comes from pi-atm's model snapshot (fake-b's
    // 32000), not the unknown model's 200K default. Wait for the final
    // display: the first `context` frame already shows `0.0% (0/32K)`.
    let expected = "3.2% (1.0K/32K)";
    let sessions = sessions_when(
        &registry,
        |s| matches!(s, [one] if one.context_display == expected),
    )
    .await;
    let displays: Vec<&str> = sessions
        .iter()
        .map(|s| s.context_display.as_str())
        .collect();
    assert_eq!(displays, [expected]);

    client.send(ClientMessage::pi_event(last.clone())).await;
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
    old.send(ClientMessage::pi_event(start.clone())).await;
    let old_id = only_session(&registry).await.id;
    assert_eq!(Some(old_id.as_str()), start["session_id"].as_str());

    old.send(ClientMessage::pi_event(end_old.clone())).await;
    assert_no_sessions(&registry).await;

    let mut new = server.connect().await;
    new.handshake(None).await;
    new.send(ClientMessage::pi_event(start_new.clone())).await;
    let new_id = only_session(&registry).await.id;
    assert_eq!(Some(new_id.as_str()), start_new["session_id"].as_str());
    assert_ne!(new_id, old_id);

    new.send(ClientMessage::pi_event(end.clone())).await;
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
    let mut client = server.connect().await;
    client.handshake(None).await;
    for event in rest {
        client.send(ClientMessage::pi_event(event.clone())).await;
    }

    let done = |s: &[SessionView]| matches!(s, [one] if one.cost_usd == 0.01065);
    let sessions = sessions_when(&registry, done).await;
    let shown: Vec<(f64, &str)> = sessions
        .iter()
        .map(|s| (s.cost_usd, s.context_display.as_str()))
        .collect();
    assert_eq!(shown, [(0.01065, "3.8% (1.2K/32K)")]);

    server.shutdown().await;
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

    sleep(Duration::from_millis(50)).await;

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

    sleep(Duration::from_millis(50)).await;

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

/// The hook payloads in one of atm-codex-adapter's captured codex-cli
/// 0.163.0 fixtures, as codex wrote them to the hook's stdin, each given this
/// test's pid as atm-codex-hook would add one.
///
/// `None` outside the workspace: atmd's tests ship in its crates.io
/// package without the adapter's fixtures, so the caller skips.
fn captured_codex_events(fixture: &str) -> Option<Vec<serde_json::Value>> {
    let adapter = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../atm-codex-adapter");
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

/// Sends one codex hook payload, then a ping, and returns the sessions
/// once the Pong arrives. atmd handles one connection's messages in order
/// and awaits the registry for each lifecycle event, so the Pong means
/// the payload has been applied.
async fn apply_codex_event(
    client: &mut TestClient,
    registry: &atmd::registry::RegistryHandle,
    seq: u64,
    event: &serde_json::Value,
) -> Vec<SessionView> {
    client.send(ClientMessage::codex_event(event.clone())).await;
    client.send(ClientMessage::ping(seq)).await;
    match client.recv().await {
        DaemonMessage::Pong { seq: got } if got == seq => {}
        other => panic!("expected Pong {seq}, got {other:?}"),
    }
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
