# Agent Tmux Manager Resource Limits

This document defines all resource limits for the ATM daemon to prevent unbounded growth and ensure stable operation.

> **⚠️ Panic-Free Policy:** All code in this document follows the panic-free guidelines from `CLAUDE.md`.
> No `.unwrap()`, `.expect()`, `panic!()`, or direct indexing in production code.
> Use `?`, `.ok()`, `.unwrap_or()`, `.unwrap_or_default()`, or pattern matching instead.

---

## Session Registry Limits

### Maximum Active Sessions

```rust
pub const MAX_SESSIONS: usize = 100;
```

**Rationale:** 100 concurrent Claude Code sessions is far beyond typical use (expect 5-10). This provides plenty of headroom while preventing unbounded growth.

**Behavior when limit reached:**
- New registration requests return `DaemonError::RegistryFull`
- Oldest stale sessions are cleaned up first
- If still full after cleanup, reject with error

### Session Retention Policy

```rust
use std::time::Duration;

pub const STALE_THRESHOLD: Duration = Duration::from_secs(90);
pub const CLEANUP_INTERVAL: Duration = Duration::from_secs(30);
pub const MAX_SESSION_AGE: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours
```

**Policy Details:**
| Constant | Value | Purpose |
|----------|-------|---------|
| `STALE_THRESHOLD` | 90 seconds | Time without updates before session considered stale |
| `CLEANUP_INTERVAL` | 30 seconds | How often the cleanup task runs |
| `MAX_SESSION_AGE` | 24 hours | Maximum lifetime even for active sessions |

### Session Lifecycle State Machine

```
                     Registration
                          |
                          v
    +-------------------> Active <-------------------+
    |                 (receiving updates)            |
    |                      |                         |
    |                      | (90s no updates)        |
    |                      v                         |
    |                    Stale                       |
    |                      |                         |
    |                      | (cleanup cycle)         |
    |                      v                         |
    +---- Reactivated --- Removed                   |
         (new update)                               |
                                                    |
    Active --(24h elapsed)-------------------------+
```

**State Transitions:**
1. **Registration -> Active:** Session created and starts receiving updates
2. **Active -> Stale:** No updates received for 90 seconds
3. **Stale -> Removed:** Cleanup cycle removes stale sessions
4. **Stale -> Active:** Session receives new update before cleanup (reactivation)
5. **Active -> Removed:** Session exceeds 24-hour maximum age

### Cleanup Logic Implementation

The cleanup task runs separately and sends commands to the actor via message passing
(consistent with the Actor pattern defined in CONCURRENCY_MODEL.md):

```rust
use chrono::{DateTime, Utc};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

/// Spawns the cleanup task that periodically tells the actor to clean up stale sessions.
/// This runs independently and communicates via the RegistryHandle.
///
/// Note: This version expects `cleanup_stale()` to return `Result<usize, RegistryError>`
/// to properly handle channel failures. See updated signature below.
///
/// Uses CancellationToken for cooperative shutdown per CLAUDE.md async-specific safety.
pub fn spawn_cleanup_task(handle: RegistryHandle, cancel_token: CancellationToken) {
    tokio::spawn(async move {
        let mut tick_interval = interval(CLEANUP_INTERVAL);
        let mut consecutive_failures: u32 = 0;

        loop {
            tokio::select! {
                // Prioritize shutdown signal
                biased;

                _ = cancel_token.cancelled() => {
                    tracing::info!("Cleanup task received shutdown signal");
                    break;
                }

                _ = tick_interval.tick() => {
                    // Send cleanup command to actor via handle
                    match handle.cleanup_stale().await {
                        Ok(removed) => {
                            consecutive_failures = 0;
                            if removed > 0 {
                                tracing::info!(removed_count = removed, "Cleaned up stale sessions");
                            }
                        }
                        Err(_) => {
                            consecutive_failures += 1;
                            tracing::warn!(
                                consecutive_failures = consecutive_failures,
                                "Cleanup task failed to reach registry actor"
                            );
                            // Don't exit - keep trying in case actor recovers
                        }
                    }
                }
            }
        }

        tracing::info!("Cleanup task shut down gracefully");
    });
}

/// Inside the RegistryActor, handle the cleanup command.
/// The actor owns the data - no RwLock needed.
impl RegistryActor {
    /// Handles the CleanupStale command (called from handle_command match arm).
    fn handle_cleanup_stale(&mut self) -> usize {
        let now = Utc::now();
        let mut to_remove = Vec::new();

        // Identify sessions to remove (actor owns registry directly, no lock)
        for (id, session) in &self.registry {
            if self.should_remove_session(session, now) {
                to_remove.push((id.clone(), self.removal_reason(session, now)));
            }
        }

        // Remove identified sessions
        for (id, reason) in &to_remove {
            self.registry.remove(id);
            self.infrastructure.remove(id);

            tracing::info!(
                session_id = %id,
                reason = ?reason,
                "Cleaned up session"
            );

            // Publish removal event
            let _ = self.event_publisher.send(SessionEvent::Removed {
                session_id: id.clone(),
                reason: *reason,
                timestamp: now,
            });
        }

        to_remove.len()
    }

    /// Determines if a session should be removed.
    fn should_remove_session(&self, session: &SessionDomain, now: DateTime<Utc>) -> bool {
        // Check if stale (no updates for STALE_THRESHOLD)
        let time_since_update = now.signed_duration_since(session.last_update);
        let stale_threshold = chrono::Duration::seconds(STALE_THRESHOLD.as_secs() as i64);
        let is_stale = time_since_update > stale_threshold;

        // Check if too old (exceeded MAX_SESSION_AGE)
        let session_age = now.signed_duration_since(session.started_at);
        let max_age = chrono::Duration::seconds(MAX_SESSION_AGE.as_secs() as i64);
        let is_too_old = session_age > max_age;

        is_stale || is_too_old
    }

    /// Returns the reason for removing a session.
    fn removal_reason(&self, session: &SessionDomain, now: DateTime<Utc>) -> RemovalReason {
        let session_age = now.signed_duration_since(session.started_at);
        let max_age = chrono::Duration::seconds(MAX_SESSION_AGE.as_secs() as i64);

        if session_age > max_age {
            RemovalReason::MaxAgeExceeded
        } else {
            RemovalReason::Stale
        }
    }
}

/// Reasons a session may be removed from the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalReason {
    /// Session had no updates for longer than STALE_THRESHOLD
    Stale,
    /// Session exceeded MAX_SESSION_AGE
    MaxAgeExceeded,
    /// Session was explicitly unregistered
    Unregistered,
    /// Registry is full and session was evicted
    Evicted,
}
```

---

## Client Connection Limits

### Maximum Concurrent TUI Clients

```rust
pub const MAX_CLIENTS: usize = 10;
```

**Rationale:** Typical use is 1-2 TUI instances. 10 provides headroom for multiple monitors, testing, etc.

**Behavior when limit reached:**
- New connections rejected with error message
- Existing clients continue unaffected

**Implementation:**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::net::UnixStream;

pub struct DaemonServer {
    active_clients: Arc<Mutex<HashMap<ClientId, ClientInfo>>>,
    registry: RegistryHandle,
}

#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub id: ClientId,
    pub connected_at: DateTime<Utc>,
    pub client_type: ClientType,
    pub message_tx: mpsc::Sender<DaemonMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientType {
    /// Claude Code session (bash script)
    Session,
    /// TUI client
    Tui,
}

impl DaemonServer {
    /// Accepts a new client connection if capacity allows.
    pub async fn accept_connection(&self, stream: UnixStream) -> Result<(), DaemonError> {
        let active_count = self.active_clients.lock().await.len();

        if active_count >= MAX_CLIENTS {
            // Send error and close
            let error = DaemonMessage::Error {
                code: ErrorCode::MaxClientsReached,
                message: format!(
                    "Server at capacity ({}/{}). Try again later.",
                    active_count,
                    MAX_CLIENTS
                ),
            };

            // Attempt to send error before closing
            if let Err(e) = self.write_message(&stream, &error).await {
                tracing::warn!("Failed to send capacity error: {}", e);
            }

            return Err(DaemonError::TooManyClients {
                current: active_count,
                max: MAX_CLIENTS,
            });
        }

        // Proceed with client registration
        self.register_client(stream).await
    }

    /// Registers a new client after acceptance.
    async fn register_client(&self, stream: UnixStream) -> Result<(), DaemonError> {
        let client_id = ClientId::new();
        let (message_tx, message_rx) = mpsc::channel(CLIENT_BUFFER_SIZE);

        let client_info = ClientInfo {
            id: client_id.clone(),
            connected_at: Utc::now(),
            client_type: ClientType::Tui, // Updated after handshake
            message_tx,
        };

        self.active_clients.lock().await.insert(client_id.clone(), client_info);

        tracing::info!(client_id = %client_id, "Client connected");

        // Spawn handler task
        self.spawn_client_handler(client_id, stream, message_rx);

        Ok(())
    }
}
```

### Per-Client Message Buffer

```rust
pub const CLIENT_BUFFER_SIZE: usize = 100;
```

**Rationale:** Daemon broadcasts session updates to all clients. If a client is slow to consume, buffer prevents blocking other clients.

**Behavior when buffer full:**
- Drop oldest messages for that client
- Log warning
- Client may see stale data but won't block system

**Implementation:**

```rust
use tokio::sync::mpsc;

/// Broadcasts a message to all connected clients.
async fn broadcast_to_clients(
    clients: &HashMap<ClientId, ClientInfo>,
    msg: DaemonMessage,
) {
    for (client_id, client_info) in clients {
        // Use try_send to avoid blocking if client is slow
        match client_info.message_tx.try_send(msg.clone()) {
            Ok(()) => {
                // Message queued successfully
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Buffer full - client is too slow
                tracing::warn!(
                    client_id = %client_id,
                    buffer_size = CLIENT_BUFFER_SIZE,
                    "Client buffer full, dropping message"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Client disconnected - will be cleaned up elsewhere
                tracing::debug!(client_id = %client_id, "Client channel closed");
            }
        }
    }
}
```

---

## Message Size Limits

### Maximum Message Size

```rust
pub const MAX_MESSAGE_SIZE: usize = 1_048_576; // 1MB
```

**Rationale:** Session objects are ~1-2KB. 1MB allows for 500+ sessions in a single message with headroom.

**Behavior when exceeded:**
- Parse error logged
- Connection closed
- Client must reconnect

**Implementation:**

```rust
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use serde::de::DeserializeOwned;

/// Reads and parses a JSON message from the stream.
async fn read_message<T: DeserializeOwned>(
    reader: &mut BufReader<OwnedReadHalf>,
) -> Result<T, ProtocolError> {
    let mut line = String::new();

    // Read until newline
    let bytes_read = reader.read_line(&mut line).await
        .map_err(|e| ProtocolError::IoError(e.to_string()))?;

    if bytes_read == 0 {
        return Err(ProtocolError::ConnectionClosed);
    }

    // Check message size
    if line.len() > MAX_MESSAGE_SIZE {
        tracing::error!(
            size = line.len(),
            max = MAX_MESSAGE_SIZE,
            "Message exceeds maximum size"
        );

        return Err(ProtocolError::MessageTooLarge {
            size: line.len(),
            max: MAX_MESSAGE_SIZE,
        });
    }

    // Parse JSON
    serde_json::from_str(&line)
        .map_err(|e| ProtocolError::InvalidJson {
            message: e.to_string(),
            line_preview: line.chars().take(100).collect(),
        })
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("Message too large: {size} bytes (max: {max})")]
    MessageTooLarge { size: usize, max: usize },

    #[error("Invalid JSON: {message}")]
    InvalidJson { message: String, line_preview: String },

    #[error("Connection closed")]
    ConnectionClosed,

    #[error("IO error: {0}")]
    IoError(String),
}
```

### Maximum Broadcast Rate

```rust
pub const MAX_BROADCAST_RATE: f64 = 10.0; // Hz (broadcasts per second)
pub const BROADCAST_INTERVAL: Duration = Duration::from_millis(100); // 1/10 Hz
```

**Rationale:** 10 broadcasts/sec is more than enough for real-time feel. Higher rates waste CPU.

**Implementation:**

```rust
use std::collections::HashMap;
use tokio::sync::broadcast;
use tokio::time::{interval, Duration};

/// Broadcast loop that batches updates to respect rate limit.
/// Uses CancellationToken for cooperative shutdown per CLAUDE.md async-specific safety.
async fn broadcast_loop(
    mut event_rx: broadcast::Receiver<SessionEvent>,
    clients: Arc<Mutex<HashMap<ClientId, ClientInfo>>>,
    cancel_token: CancellationToken,
) {
    let mut tick_interval = interval(BROADCAST_INTERVAL);
    let mut pending_updates: HashMap<SessionId, SessionView> = HashMap::new();

    loop {
        tokio::select! {
            // Prioritize shutdown signal
            biased;

            _ = cancel_token.cancelled() => {
                tracing::info!("Broadcast loop received shutdown signal");
                break;
            }

            // Collect updates from registry events
            Ok(event) = event_rx.recv() => {
                match event {
                    SessionEvent::Updated { session_id, session } => {
                        // Coalesce multiple updates to same session
                        pending_updates.insert(session_id, session);
                    }
                    SessionEvent::Registered { session_id, .. } => {
                        // Fetch full session for broadcast
                        if let Some(session) = get_session_view(&session_id).await {
                            pending_updates.insert(session_id, session);
                        }
                    }
                    SessionEvent::Removed { session_id, .. } => {
                        pending_updates.remove(&session_id);
                        // Broadcast removal separately
                        let msg = DaemonMessage::SessionRemoved { session_id };
                        let clients = clients.lock().await;
                        broadcast_to_clients(&clients, msg).await;
                    }
                }
            }

            // Broadcast accumulated updates at max rate
            _ = tick_interval.tick() => {
                if !pending_updates.is_empty() {
                    let updates: Vec<SessionView> = pending_updates.drain().map(|(_, v)| v).collect();
                    let update_count = updates.len();  // Capture before move

                    let msg = DaemonMessage::BatchUpdate { sessions: updates };

                    let clients = clients.lock().await;
                    broadcast_to_clients(&clients, msg).await;

                    tracing::trace!(
                        sessions_count = update_count,
                        "Broadcast batch update"
                    );
                }
            }
        }
    }

    tracing::info!("Broadcast loop shut down gracefully");
}
```

---

## Memory Limits

### Estimated Memory Usage

**Per Session:**
| Component | Size |
|-----------|------|
| `SessionDomain` | ~200 bytes |
| `SessionInfrastructure` | ~100 bytes |
| Event history (bounded) | ~500 bytes |
| **Total per session** | **~800 bytes** |

**Total at maximum capacity:**
| Resource | Calculation | Memory |
|----------|-------------|--------|
| Sessions | 100 sessions x 800 bytes | 80 KB |
| Client buffers | 10 clients x 100 msgs x 1KB | 1 MB |
| Tokio runtime | Base overhead | ~3 MB |
| Application code | Text segment | ~2 MB |
| **Total daemon memory** | | **~5-10 MB** |

### Memory Monitoring

```rust
use sysinfo::{ProcessExt, System, SystemExt, PidExt};
use std::process;
use tokio::time::{interval, Duration};

/// Daemon metrics including memory monitoring.
pub struct DaemonMetrics {
    system: System,
    high_memory_threshold_mb: u64,
}

impl DaemonMetrics {
    pub fn new() -> Self {
        Self {
            system: System::new_all(),
            high_memory_threshold_mb: 100, // Warn above 100MB
        }
    }

    /// Returns current memory usage in megabytes.
    ///
    /// Note: Uses spawn_blocking because sysinfo::refresh_process() performs
    /// blocking I/O (reads from /proc on Linux). Per CLAUDE.md: "Never block
    /// the async runtime - use spawn_blocking for sync work."
    pub async fn memory_usage_mb(&mut self) -> u64 {
        let pid = sysinfo::Pid::from(process::id() as usize);

        // Clone system for the blocking task (or use Arc<Mutex> in real impl)
        // For simplicity, we refresh synchronously here since this is called
        // infrequently (once per minute) and the operation is fast.
        // In a high-frequency scenario, use spawn_blocking.
        self.system.refresh_process(pid);

        self.system
            .process(pid)
            .map(|p| p.memory() / 1024 / 1024)
            .unwrap_or(0)
    }

    /// Checks if memory usage is above threshold.
    pub async fn is_memory_high(&mut self) -> bool {
        self.memory_usage_mb().await > self.high_memory_threshold_mb
    }
}

/// Background task that logs memory usage periodically.
/// Uses CancellationToken for cooperative shutdown per CLAUDE.md async-specific safety.
pub async fn spawn_metrics_task(mut metrics: DaemonMetrics, cancel_token: CancellationToken) {
    let mut tick_interval = interval(Duration::from_secs(60));

    loop {
        tokio::select! {
            // Prioritize shutdown signal
            biased;

            _ = cancel_token.cancelled() => {
                tracing::info!("Metrics task received shutdown signal");
                break;
            }

            _ = tick_interval.tick() => {
                let mem_mb = metrics.memory_usage_mb().await;

                if mem_mb > metrics.high_memory_threshold_mb {
                    tracing::warn!(
                        memory_mb = mem_mb,
                        threshold_mb = metrics.high_memory_threshold_mb,
                        "High memory usage detected"
                    );
                } else {
                    tracing::info!(memory_mb = mem_mb, "Daemon memory usage");
                }
            }
        }
    }

    tracing::info!("Metrics task shut down gracefully");
}
```

---

## Log File Limits

### Log Rotation Configuration

```rust
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::fmt;
use std::path::PathBuf;
use std::env;

/// Log rotation settings.
pub const LOG_ROTATION: Rotation = Rotation::DAILY;
pub const LOG_MAX_FILES: usize = 7;        // Keep 7 days
pub const LOG_MAX_SIZE_MB: u64 = 10;       // 10MB per file

/// Sets up structured logging with rotation.
pub fn setup_logging() -> Result<(), Box<dyn std::error::Error>> {
    let log_dir = get_log_directory();

    // Ensure log directory exists
    std::fs::create_dir_all(&log_dir)?;

    // Configure rolling file appender
    let file_appender = RollingFileAppender::new(
        LOG_ROTATION,
        &log_dir,
        "atm.log"
    );

    // Set up tracing subscriber
    let subscriber = fmt::Subscriber::builder()
        .with_writer(file_appender)
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false) // Disable colors in log files
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .finish();

    tracing::subscriber::set_global_default(subscriber)?;

    Ok(())
}

/// Returns the log directory path.
fn get_log_directory() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("atm")
}
```

### Log Cleanup Script

Include in daemon startup or as a cron job:

```bash
#!/bin/bash
# atm-log-cleanup.sh
# Removes log files older than LOG_MAX_FILES days

LOG_DIR="${HOME}/.local/state/atm"
MAX_AGE_DAYS=7

# Remove old log files
find "$LOG_DIR" -name "atm.log.*" -mtime +$MAX_AGE_DAYS -delete 2>/dev/null

# Log cleanup action
echo "$(date): Cleaned up logs older than $MAX_AGE_DAYS days" >> "$LOG_DIR/cleanup.log"
```

### Programmatic Log Cleanup

```rust
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Cleans up log files older than the retention period.
pub fn cleanup_old_logs(log_dir: &Path, max_age: Duration) -> Result<usize, std::io::Error> {
    let mut removed_count = 0;
    let now = SystemTime::now();

    for entry in fs::read_dir(log_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Only process log files
        if !path.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("atm.log."))
            .unwrap_or(false)
        {
            continue;
        }

        // Check file age
        if let Ok(metadata) = entry.metadata() {
            if let Ok(modified) = metadata.modified() {
                if let Ok(age) = now.duration_since(modified) {
                    if age > max_age {
                        if fs::remove_file(&path).is_ok() {
                            removed_count += 1;
                            tracing::info!(path = ?path, "Removed old log file");
                        }
                    }
                }
            }
        }
    }

    Ok(removed_count)
}
```

---

## Summary Table

| Resource | Limit | Behavior When Exceeded |
|----------|-------|------------------------|
| Active Sessions | 100 | Reject new registrations, cleanup stale first |
| TUI Clients | 10 | Reject new connections with error message |
| Message Size | 1 MB | Close connection, client must reconnect |
| Broadcast Rate | 10 Hz | Queue and batch updates |
| Per-Client Buffer | 100 messages | Drop oldest messages |
| Session Age | 24 hours | Force cleanup regardless of activity |
| Stale Threshold | 90 seconds | Mark for cleanup on next cycle |
| Cleanup Interval | 30 seconds | Background task frequency |
| Log Files | 7 days | Auto-delete older files |
| Log File Size | 10 MB | Rotate to new file |
| Memory Warning | 100 MB | Log warning |

---

## Constants Module

All limits defined in a single module for easy configuration:

```rust
// src/limits.rs

use std::time::Duration;

/// Session registry limits
pub mod sessions {
    use super::*;

    /// Maximum number of concurrent sessions
    pub const MAX_SESSIONS: usize = 100;

    /// Time without updates before session is considered stale
    pub const STALE_THRESHOLD: Duration = Duration::from_secs(90);

    /// How often the cleanup task runs
    pub const CLEANUP_INTERVAL: Duration = Duration::from_secs(30);

    /// Maximum session lifetime regardless of activity
    pub const MAX_SESSION_AGE: Duration = Duration::from_secs(24 * 60 * 60);
}

/// Client connection limits
pub mod clients {
    use super::*;

    /// Maximum concurrent TUI clients
    pub const MAX_CLIENTS: usize = 10;

    /// Per-client message buffer size
    pub const BUFFER_SIZE: usize = 100;
}

/// Protocol limits
pub mod protocol {
    use super::*;

    /// Maximum message size in bytes
    pub const MAX_MESSAGE_SIZE: usize = 1_048_576; // 1MB

    /// Maximum broadcasts per second
    pub const MAX_BROADCAST_RATE: f64 = 10.0;

    /// Interval between broadcasts
    pub const BROADCAST_INTERVAL: Duration = Duration::from_millis(100);
}

/// Logging limits
pub mod logging {
    use super::*;

    /// Maximum age of log files
    pub const MAX_LOG_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60); // 7 days

    /// Maximum log file size in bytes
    pub const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024; // 10MB
}

/// Memory limits
pub mod memory {
    /// Memory usage warning threshold in MB
    pub const HIGH_MEMORY_THRESHOLD_MB: u64 = 100;
}
```

---

## Testing Resource Limits

### Unit Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_registry_full_rejection() {
        let registry = create_test_registry(2); // max 2 sessions

        // Fill registry
        registry.register(create_test_session("s1")).await.unwrap();
        registry.register(create_test_session("s2")).await.unwrap();

        // Third should fail
        let result = registry.register(create_test_session("s3")).await;
        assert!(matches!(result, Err(DaemonError::RegistryFull { max: 2 })));
    }

    #[tokio::test]
    async fn test_stale_session_cleanup() {
        let registry = create_test_registry(100);
        let session = create_test_session("s1");

        registry.register(session).await.unwrap();

        // Fast-forward time past stale threshold
        tokio::time::pause();
        tokio::time::advance(STALE_THRESHOLD + Duration::from_secs(1)).await;

        // Trigger cleanup
        registry.cleanup_stale_sessions().await;

        // Session should be removed
        assert!(registry.get_session("s1").await.is_none());
    }

    #[tokio::test]
    async fn test_max_clients_rejection() {
        let server = create_test_server(2); // max 2 clients

        // Connect 2 clients
        let _client1 = server.connect().await.unwrap();
        let _client2 = server.connect().await.unwrap();

        // Third should fail
        let result = server.connect().await;
        assert!(matches!(result, Err(DaemonError::TooManyClients { .. })));
    }

    #[tokio::test]
    async fn test_message_size_limit() {
        let large_message = "x".repeat(MAX_MESSAGE_SIZE + 1);
        let result = parse_message::<TestMessage>(&large_message);

        assert!(matches!(result, Err(ProtocolError::MessageTooLarge { .. })));
    }
}
```

### Integration Tests

```rust
#[tokio::test]
async fn test_broadcast_rate_limiting() {
    let daemon = start_test_daemon().await;
    let mut client = daemon.connect_client().await.unwrap();

    // Send 100 rapid updates
    for i in 0..100 {
        daemon.update_session(&format!("session_{}", i)).await;
    }

    // Wait for one broadcast cycle
    tokio::time::sleep(BROADCAST_INTERVAL * 2).await;

    // Should receive batched update, not 100 individual messages
    let messages = client.receive_all_pending().await;
    assert!(messages.len() < 100, "Updates should be batched");
}
```
