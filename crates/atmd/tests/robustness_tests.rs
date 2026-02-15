//! Robustness tests for Week 3 - Daemon Polish.
//!
//! These tests verify the daemon handles edge cases and error conditions gracefully:
//! - Malformed messages
//! - Message size limits
//! - Rapid connect/disconnect
//! - High-frequency updates
//! - Recovery after errors
//!
//! Per CLAUDE.md: Tests CAN use `.unwrap()` and `.expect()` - this is allowed.

use std::path::PathBuf;
use std::time::Duration;

use atm_core::{AgentType, Model, SessionDomain, SessionId};
use atm_protocol::{ClientMessage, DaemonMessage, MessageType, ProtocolVersion};
use atmd::registry::spawn_registry;
use atmd::server::DaemonServer;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

// ============================================================================
// Constants
// ============================================================================

const SOCKET_WAIT_TIMEOUT: Duration = Duration::from_millis(500);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(10);
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_millis(100);

// ============================================================================
// Test Helpers
// ============================================================================

struct TestServer {
    socket_path: PathBuf,
    cancel_token: CancellationToken,
    _temp_dir: TempDir,
}

impl TestServer {
    async fn spawn() -> Self {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let socket_path = temp_dir.path().join("test.sock");

        let registry = spawn_registry();
        let cancel_token = CancellationToken::new();

        let server = DaemonServer::new(socket_path.clone(), registry, cancel_token.clone());

        tokio::spawn(async move {
            let _ = server.run().await;
        });

        let start = tokio::time::Instant::now();
        while start.elapsed() < SOCKET_WAIT_TIMEOUT {
            if socket_path.exists() {
                break;
            }
            sleep(SOCKET_POLL_INTERVAL).await;
        }

        assert!(socket_path.exists(), "Server socket did not appear");

        TestServer {
            socket_path,
            cancel_token,
            _temp_dir: temp_dir,
        }
    }

    async fn connect(&self) -> TestClient {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .expect("connect to server");
        TestClient::new(stream)
    }

    #[allow(dead_code)]
    async fn connect_raw(
        &self,
    ) -> (
        BufReader<tokio::net::unix::OwnedReadHalf>,
        tokio::net::unix::OwnedWriteHalf,
    ) {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .expect("connect to server");
        let (reader, writer) = stream.into_split();
        (BufReader::new(reader), writer)
    }

    async fn shutdown(self) {
        self.cancel_token.cancel();
        sleep(SHUTDOWN_GRACE_PERIOD).await;
    }
}

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

    async fn send(&mut self, msg: ClientMessage) {
        let json = serde_json::to_string(&msg).unwrap();
        self.writer.write_all(json.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn send_raw(&mut self, data: &[u8]) {
        self.writer.write_all(data).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn recv(&mut self) -> DaemonMessage {
        let mut line = String::new();
        self.reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn recv_timeout(&mut self, duration: Duration) -> Option<DaemonMessage> {
        match timeout(duration, async {
            let mut line = String::new();
            self.reader.read_line(&mut line).await.ok()?;
            serde_json::from_str(&line).ok()
        })
        .await
        {
            Ok(Some(msg)) => Some(msg),
            _ => None,
        }
    }

    async fn handshake(&mut self, client_id: Option<String>) -> String {
        self.send(ClientMessage::connect(client_id)).await;
        match self.recv().await {
            DaemonMessage::Connected { client_id, .. } => client_id,
            other => panic!("Expected Connected, got {other:?}"),
        }
    }
}

#[allow(dead_code)]
fn create_test_session(id: &str) -> SessionDomain {
    SessionDomain::new(
        SessionId::new(id),
        AgentType::GeneralPurpose,
        Model::Sonnet4,
    )
}

// ============================================================================
// Malformed Message Tests
// ============================================================================

#[tokio::test]
async fn test_malformed_json_handled_gracefully() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    // Send invalid JSON
    client.send_raw(b"this is not valid json\n").await;

    // Should receive error (connection may close after)
    // The key point is daemon doesn't crash
    sleep(Duration::from_millis(50)).await;

    // Server should still be accepting connections
    let mut client2 = server.connect().await;
    client2.handshake(Some("after-malformed".to_string())).await;

    server.shutdown().await;
}

#[tokio::test]
async fn test_empty_line_handled() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    // Send empty line
    client.send_raw(b"\n").await;

    // Small delay to let server process
    sleep(Duration::from_millis(50)).await;

    // Server should still work
    let mut client2 = server.connect().await;
    client2.handshake(None).await;

    server.shutdown().await;
}

#[tokio::test]
async fn test_partial_json_handled() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    // Send partial JSON (no newline, incomplete object)
    client.send_raw(b"{\"protocol_version\"").await;

    // Send the rest on a new line (should parse as separate message)
    sleep(Duration::from_millis(50)).await;
    client.send_raw(b"\n").await;

    // Server should still be functional
    sleep(Duration::from_millis(50)).await;

    let mut client2 = server.connect().await;
    let id = client2.handshake(None).await;
    assert!(id.starts_with("client-"));

    server.shutdown().await;
}

#[tokio::test]
async fn test_unknown_message_type_handled() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    // First do proper handshake
    client
        .handshake(Some("unknown-type-test".to_string()))
        .await;

    // Now send message with unknown/invalid message type via raw bytes
    // (valid JSON structure but serde will fail to deserialize unknown variant)
    let unknown =
        r#"{"protocol_version":{"major":1,"minor":0},"message":{"UnknownType":{"data":"test"}}}"#;
    client.send_raw(unknown.as_bytes()).await;
    client.send_raw(b"\n").await;

    // Should receive an error or connection may close
    // The key is daemon doesn't crash
    sleep(Duration::from_millis(100)).await;

    // Server should still be working for new connections
    let mut client2 = server.connect().await;
    client2.handshake(None).await;

    server.shutdown().await;
}

// ============================================================================
// Message Size Limit Tests
// ============================================================================

#[tokio::test]
async fn test_oversized_message_rejected() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    // Create a message larger than MAX_MESSAGE_SIZE (1MB)
    let large_data = "x".repeat(2 * 1024 * 1024); // 2MB
    let large_json = format!(
        r#"{{"protocol_version":{{"major":1,"minor":0}},"message":{{"type":"status_update","data":{{"padding":"{large_data}"}}}}}}"#
    );

    client.send_raw(large_json.as_bytes()).await;
    client.send_raw(b"\n").await;

    // Connection should be closed or error received
    // The key is daemon doesn't crash
    sleep(Duration::from_millis(100)).await;

    // Server should still accept new connections
    let mut client2 = server.connect().await;
    client2.handshake(None).await;

    server.shutdown().await;
}

// ============================================================================
// Rapid Connect/Disconnect Tests
// ============================================================================

#[tokio::test]
async fn test_rapid_connect_disconnect() {
    let server = TestServer::spawn().await;

    // Rapidly connect and disconnect 20 times
    for i in 0..20 {
        let mut client = server.connect().await;
        client.handshake(Some(format!("rapid-{i}"))).await;
        client.send(ClientMessage::disconnect()).await;
        // Don't wait, just move on
    }

    // Small settle time
    sleep(Duration::from_millis(100)).await;

    // Server should still work
    let mut final_client = server.connect().await;
    let id = final_client.handshake(Some("final".to_string())).await;
    assert_eq!(id, "final");

    server.shutdown().await;
}

#[tokio::test]
async fn test_many_concurrent_connections() {
    let server = TestServer::spawn().await;

    // Spawn 20 concurrent connections
    let mut handles = Vec::new();
    for i in 0..20 {
        let socket_path = server.socket_path.clone();
        let handle = tokio::spawn(async move {
            let stream = UnixStream::connect(&socket_path).await.unwrap();
            let mut client = TestClient::new(stream);
            let id = client.handshake(Some(format!("concurrent-{i}"))).await;
            assert_eq!(id, format!("concurrent-{i}"));

            // Do some operations
            client.send(ClientMessage::list_sessions()).await;
            let _ = client.recv().await;

            client.send(ClientMessage::ping(i as u64)).await;
            match client.recv().await {
                DaemonMessage::Pong { seq } => assert_eq!(seq, i as u64),
                other => panic!("Expected Pong, got {other:?}"),
            }
        });
        handles.push(handle);
    }

    // All should complete successfully
    for handle in handles {
        handle.await.expect("concurrent connection should succeed");
    }

    server.shutdown().await;
}

// ============================================================================
// High-Frequency Update Tests
// ============================================================================

#[tokio::test]
async fn test_rapid_status_updates() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client
        .handshake(Some("rapid-update-test".to_string()))
        .await;

    // Use the current process PID (a real PID that set_pid can validate)
    let current_pid = std::process::id();

    // Send 50 status updates rapidly (simulating 300ms updates compressed)
    for i in 0..50 {
        let status_json = serde_json::json!({
            "session_id": "rapid-session",
            "pid": current_pid,
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.01 * (i as f64), "total_duration_ms": 1000 * i},
            "context_window": {"total_input_tokens": 100 * i, "context_window_size": 200000}
        });

        let msg = ClientMessage {
            protocol_version: ProtocolVersion::CURRENT,
            message: MessageType::StatusUpdate { data: status_json },
        };
        client.send(msg).await;
    }

    // Small settle time
    sleep(Duration::from_millis(100)).await;

    // Query to verify state is consistent
    client.send(ClientMessage::list_sessions()).await;
    match client.recv().await {
        DaemonMessage::SessionList { sessions } => {
            assert_eq!(
                sessions.len(),
                1,
                "Should have 1 session from auto-registration"
            );
            // Last update should be reflected
            assert!(sessions[0].cost_usd > 0.4, "Cost should reflect updates");
        }
        other => panic!("Expected SessionList, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn test_multiple_sessions_rapid_updates() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    // Use synthetic PIDs (high range won't conflict with real processes)
    let base_pid: u32 = 0x7000_0000;

    // Create and update 10 sessions rapidly
    for session_num in 0..10 {
        let session_pid = base_pid + session_num;
        for update_num in 0..10 {
            let status_json = serde_json::json!({
                "session_id": format!("session-{}", session_num),
                "pid": session_pid,
                "model": {"id": "claude-sonnet-4-20250514"},
                "cost": {"total_cost_usd": 0.01 * (update_num as f64), "total_duration_ms": 1000},
                "context_window": {"total_input_tokens": 100, "context_window_size": 200000}
            });

            let msg = ClientMessage {
                protocol_version: ProtocolVersion::CURRENT,
                message: MessageType::StatusUpdate { data: status_json },
            };
            client.send(msg).await;
        }
    }

    sleep(Duration::from_millis(100)).await;

    // Verify all sessions exist
    client.send(ClientMessage::list_sessions()).await;
    match client.recv().await {
        DaemonMessage::SessionList { sessions } => {
            assert_eq!(sessions.len(), 10, "Should have 10 sessions");
        }
        other => panic!("Expected SessionList, got {other:?}"),
    }

    server.shutdown().await;
}

// ============================================================================
// Error Recovery Tests
// ============================================================================

#[tokio::test]
async fn test_client_continues_after_error() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    // Send an invalid status update (missing required fields)
    let invalid_status = serde_json::json!({
        "not_session_id": "missing-session-id"
    });

    let msg = ClientMessage {
        protocol_version: ProtocolVersion::CURRENT,
        message: MessageType::StatusUpdate {
            data: invalid_status,
        },
    };
    client.send(msg).await;

    // Should receive error
    match client.recv().await {
        DaemonMessage::Error { .. } => {}
        other => panic!("Expected Error, got {other:?}"),
    }

    // Client should still be able to send valid messages
    client.send(ClientMessage::ping(42)).await;
    match client.recv().await {
        DaemonMessage::Pong { seq } => assert_eq!(seq, 42),
        other => panic!("Expected Pong after error, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn test_multiple_errors_dont_break_connection() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    // Send multiple invalid messages
    for _ in 0..5 {
        let invalid = serde_json::json!({ "invalid": "data" });
        let msg = ClientMessage {
            protocol_version: ProtocolVersion::CURRENT,
            message: MessageType::StatusUpdate { data: invalid },
        };
        client.send(msg).await;

        // Drain error response
        let _ = client.recv_timeout(Duration::from_millis(100)).await;
    }

    // Connection should still work
    client.send(ClientMessage::list_sessions()).await;
    match client.recv().await {
        DaemonMessage::SessionList { .. } => {}
        other => panic!("Expected SessionList after errors, got {other:?}"),
    }

    server.shutdown().await;
}

// ============================================================================
// Subscriber Broadcast Tests
// ============================================================================

#[tokio::test]
async fn test_subscriber_receives_session_updates() {
    let server = TestServer::spawn().await;

    // Client 1: subscriber
    let mut subscriber = server.connect().await;
    subscriber.handshake(Some("subscriber".to_string())).await;
    subscriber.send(ClientMessage::subscribe(None)).await;

    // Receive initial session list
    match subscriber.recv().await {
        DaemonMessage::SessionList { sessions } => {
            assert_eq!(sessions.len(), 0);
        }
        other => panic!("Expected initial SessionList, got {other:?}"),
    }

    // Client 2: status updater
    let mut updater = server.connect().await;
    updater.handshake(Some("updater".to_string())).await;

    // Use the current process PID (a real PID that set_pid can validate)
    let current_pid = std::process::id();

    // Send status update to create a session
    let status_json = serde_json::json!({
        "session_id": "broadcast-test-session",
        "pid": current_pid,
        "model": {"id": "claude-sonnet-4-20250514"},
        "cost": {"total_cost_usd": 0.05, "total_duration_ms": 5000},
        "context_window": {"total_input_tokens": 1000, "context_window_size": 200000}
    });

    let msg = ClientMessage {
        protocol_version: ProtocolVersion::CURRENT,
        message: MessageType::StatusUpdate { data: status_json },
    };
    updater.send(msg).await;

    // Subscriber should receive the update event
    let event = subscriber.recv_timeout(Duration::from_secs(2)).await;
    assert!(event.is_some(), "Subscriber should receive update event");

    match event.unwrap() {
        DaemonMessage::SessionUpdated { session } => {
            assert_eq!(session.id.as_str(), "broadcast-test-session");
        }
        other => panic!("Expected SessionUpdated, got {other:?}"),
    }

    server.shutdown().await;
}

// ============================================================================
// Edge Cases
// ============================================================================

#[tokio::test]
async fn test_subscribe_before_sessions_exist() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    // Subscribe when no sessions exist
    client.send(ClientMessage::subscribe(None)).await;

    match client.recv().await {
        DaemonMessage::SessionList { sessions } => {
            assert!(sessions.is_empty(), "Should be empty initially");
        }
        other => panic!("Expected empty SessionList, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn test_unsubscribe_when_not_subscribed() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    // Unsubscribe without subscribing first - should not cause error
    client
        .send(ClientMessage::new(MessageType::Unsubscribe))
        .await;

    // Should still work normally
    client.send(ClientMessage::ping(1)).await;
    match client.recv().await {
        DaemonMessage::Pong { seq } => assert_eq!(seq, 1),
        other => panic!("Expected Pong, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn test_double_subscribe() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    // Subscribe twice
    client.send(ClientMessage::subscribe(None)).await;
    let _ = client.recv().await; // Initial session list

    client.send(ClientMessage::subscribe(None)).await;
    match client.recv().await {
        DaemonMessage::SessionList { .. } => {}
        other => panic!("Expected SessionList on re-subscribe, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn test_empty_session_id() {
    let server = TestServer::spawn().await;
    let mut client = server.connect().await;

    client.handshake(None).await;

    // Status update with empty session_id
    let status_json = serde_json::json!({
        "session_id": "",
        "model": {"id": "claude-sonnet-4-20250514"},
        "cost": {"total_cost_usd": 0.0, "total_duration_ms": 0},
        "context_window": {"total_input_tokens": 0, "context_window_size": 200000}
    });

    let msg = ClientMessage {
        protocol_version: ProtocolVersion::CURRENT,
        message: MessageType::StatusUpdate { data: status_json },
    };
    client.send(msg).await;

    // Should handle gracefully (either error or create session with empty id)
    // The key is daemon doesn't crash
    sleep(Duration::from_millis(50)).await;

    client.send(ClientMessage::ping(99)).await;
    match client.recv().await {
        DaemonMessage::Pong { seq } => assert_eq!(seq, 99),
        DaemonMessage::Error { .. } => {} // Also acceptable
        other => panic!("Expected Pong or Error, got {other:?}"),
    }

    server.shutdown().await;
}
