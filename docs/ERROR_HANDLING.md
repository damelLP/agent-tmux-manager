# Agent Tmux Manager Error Handling Strategy

> **⚠️ Panic-Free Policy:** All code in this document follows the panic-free guidelines from `CLAUDE.md`.
> No `.unwrap()`, `.expect()`, `panic!()`, or direct indexing in production code.
> Use `?`, `.ok()`, `.unwrap_or()`, `.unwrap_or_default()`, or pattern matching instead.
> `let _ =` is acceptable for intentional best-effort operations (shutdown cleanup, rejection responses).

## Overview

This document defines the comprehensive error handling strategy for the Agent Tmux Manager monitoring system. It covers error types, retry policies, graceful degradation, logging, and testing approaches across all components: daemon, TUI, and shell scripts.

---

## Principles

1. **Never panic in production code**
   - Return `Result`/`Option` types, never `.unwrap()` or `.expect()` in production
   - Use `?` operator for error propagation
   - Tests may use `.unwrap()` for brevity

2. **Never break Claude Code** - Bash scripts always exit 0
   - Status line and hook scripts must not interfere with Claude Code operation
   - Silent failures are acceptable; breaking Claude Code is not

3. **Observable failures** - Log all errors with context
   - Every error should be logged with enough context to diagnose
   - Use structured logging with session IDs, timestamps, and error chains

4. **Typed errors with thiserror + anyhow**
   - Use `thiserror` for errors that cross API boundaries (callers need to match)
   - Use `anyhow` for application-level code (main, CLI, config loading)
   - Never use string-only errors

5. **Error boundaries** - Contain failures to prevent cascade
   - One session's error should not affect other sessions
   - One client's malformed message should not crash the daemon

---

## thiserror vs anyhow

Agent Tmux Manager uses **both** crates for different purposes:

### When to Use thiserror

Use `thiserror` when callers need to **match on error variants** or when errors cross API boundaries:

```rust
use thiserror::Error;

// ✅ thiserror: Callers will match on these variants
#[derive(Error, Debug)]
pub enum RegistryError {
    #[error("Registry full: cannot add session (max: {max})")]
    Full { max: usize },

    #[error("Session not found: {session_id}")]
    NotFound { session_id: SessionId },
}

// Caller can handle specific cases:
match registry.register(session).await {
    Ok(()) => println!("Registered"),
    Err(RegistryError::Full { max }) => {
        // Handle capacity issue specifically
        trigger_cleanup().await;
    }
    Err(RegistryError::NotFound { .. }) => {
        // This shouldn't happen for register, but handle it
    }
}
```

### When to Use anyhow

Use `anyhow` for **application-level code** where you just want to propagate errors with context:

```rust
use anyhow::{Context, Result, bail, ensure};

// ✅ anyhow: main.rs, CLI, config loading - no one matches on these
fn main() -> Result<()> {
    let config = load_config()
        .context("Failed to load configuration")?;

    let socket_path = config.socket_path
        .as_ref()
        .context("socket_path not configured")?;

    ensure!(socket_path.exists(), "Socket path does not exist: {:?}", socket_path);

    run_daemon(config).context("Daemon failed")?;

    Ok(())
}

// Config loading - errors are just reported, not matched
fn load_config() -> Result<Config> {
    let path = dirs::config_dir()
        .context("Could not find config directory")?
        .join("atm/config.toml");

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config from {:?}", path))?;

    let config: Config = toml::from_str(&content)
        .context("Failed to parse config TOML")?;

    Ok(config)
}
```

### Combining Both

The boundary between library and application code:

```rust
// In daemon library (src/lib.rs) - uses thiserror
pub mod daemon {
    use thiserror::Error;

    #[derive(Error, Debug)]
    pub enum DaemonError {
        #[error("Registry error: {0}")]
        Registry(#[from] RegistryError),
        // ...
    }

    pub async fn start(config: Config) -> Result<(), DaemonError> {
        // Library code returns typed errors
    }
}

// In binary (src/bin/atmd.rs) - uses anyhow
use anyhow::{Context, Result};
use atm::daemon;

fn main() -> Result<()> {
    let config = load_config()?;

    // Convert typed error to anyhow at the boundary
    daemon::start(config)
        .await
        .context("Daemon failed to start")?;

    Ok(())
}
```

### Quick Reference

| Location | Crate | Reason |
|----------|-------|--------|
| `src/registry.rs` | thiserror | TUI matches on `RegistryError` variants |
| `src/protocol.rs` | thiserror | Need to handle version mismatch specifically |
| `src/daemon.rs` | thiserror | Public API, callers may handle errors |
| `src/bin/atmd.rs` | anyhow | Application entrypoint, just report errors |
| `src/bin/atm.rs` | anyhow | TUI entrypoint |
| `src/config.rs` | anyhow | Config errors just get reported |
| `tests/` | anyhow | Tests just need `.unwrap()` or `?` |

### Error Context Best Practices

```rust
use anyhow::{Context, Result};

// ❌ BAD: No context
fn bad_load() -> Result<Config> {
    let content = std::fs::read_to_string(path)?;  // "No such file" - which file?
    Ok(toml::from_str(&content)?)                   // "expected string" - where?
}

// ✅ GOOD: Context at each step
fn good_load() -> Result<Config> {
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {:?}", path))?;

    toml::from_str(&content)
        .with_context(|| format!("Failed to parse {:?} as TOML", path))
}

// Error output with context:
// Error: Failed to start daemon
//
// Caused by:
//     0: Failed to load configuration
//     1: Failed to read "/home/user/.config/atm/config.toml"
//     2: No such file or directory (os error 2)
```

---

## Error Types by Layer

### Daemon Errors

```rust
use std::io;
use std::path::PathBuf;
use thiserror::Error;

/// Top-level daemon errors
#[derive(Error, Debug)]
pub enum DaemonError {
    /// Session registry has reached maximum capacity
    #[error("Session registry is full (max: {max}, current: {current})")]
    RegistryFull { max: usize, current: usize },

    /// Requested session does not exist in registry
    #[error("Session not found: {0}")]
    SessionNotFound(SessionId),

    /// Session already exists (duplicate registration)
    #[error("Session already registered: {0}")]
    SessionAlreadyExists(SessionId),

    /// Protocol-level error during message handling
    #[error("Protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    /// I/O error (socket, file operations)
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// JSON serialization/deserialization error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Internal channel communication error
    #[error("Internal channel error: receiver dropped")]
    ChannelClosed,

    /// Client connection limit reached
    #[error("Too many clients connected (max: {max})")]
    TooManyClients { max: usize },

    /// Socket binding failed
    #[error("Failed to bind socket at {path}: {source}")]
    SocketBindError {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Daemon already running
    #[error("Daemon already running (pid: {pid}, socket: {socket_path:?})")]
    AlreadyRunning { pid: u32, socket_path: PathBuf },
}

/// Strongly-typed session identifier
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
```

### Protocol Errors

```rust
use thiserror::Error;

/// Protocol-level errors for message parsing and validation
#[derive(Error, Debug)]
pub enum ProtocolError {
    /// Client sent unsupported protocol version
    #[error("Unsupported protocol version: {client_version} (daemon supports: {supported_versions:?})")]
    UnsupportedVersion {
        client_version: String,
        supported_versions: Vec<String>,
    },

    /// Message format is invalid (not valid JSON, wrong structure)
    #[error("Invalid message format: {reason}")]
    InvalidFormat { reason: String },

    /// Required field is missing from message
    #[error("Missing required field: {field} in message type: {message_type}")]
    MissingField {
        field: String,
        message_type: String,
    },

    /// Field value is out of acceptable range
    #[error("Invalid field value: {field} = {value} (expected: {expected})")]
    InvalidFieldValue {
        field: String,
        value: String,
        expected: String,
    },

    /// Unknown message type received
    #[error("Unknown message type: {0}")]
    UnknownMessageType(String),

    /// Message exceeds size limit
    #[error("Message too large: {size} bytes (max: {max} bytes)")]
    MessageTooLarge { size: usize, max: usize },

    /// Handshake not completed before sending other messages
    #[error("Handshake required before sending {message_type} messages")]
    HandshakeRequired { message_type: String },
}
```

### TUI Errors

```rust
use std::io;
use std::path::PathBuf;
use thiserror::Error;

/// TUI application errors
#[derive(Error, Debug)]
pub enum TuiError {
    /// Lost connection to daemon
    #[error("Daemon disconnected unexpectedly")]
    DaemonDisconnected,

    /// Could not establish connection within timeout
    #[error("Connection to daemon timed out after {timeout_secs} seconds")]
    ConnectionTimeout { timeout_secs: u64 },

    /// Daemon socket does not exist
    #[error("Daemon not running (socket not found: {socket_path:?})")]
    DaemonNotRunning { socket_path: PathBuf },

    /// Error during terminal rendering
    #[error("Render error: {reason}")]
    RenderError { reason: String },

    /// Terminal state is invalid (e.g., size too small)
    #[error("Invalid terminal state: {reason}")]
    InvalidTerminalState { reason: String },

    /// Protocol version mismatch with daemon
    #[error("Protocol version mismatch (client: {client_version}, daemon: {daemon_version})")]
    VersionMismatch {
        client_version: String,
        daemon_version: String,
    },

    /// Generic I/O error
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// JSON parsing error from daemon messages
    #[error("Failed to parse daemon message: {0}")]
    ParseError(#[from] serde_json::Error),

    /// Terminal setup failed
    #[error("Failed to initialize terminal: {0}")]
    TerminalInit(String),

    /// Terminal cleanup failed
    #[error("Failed to restore terminal: {0}")]
    TerminalCleanup(String),
}
```

### Registry Errors

```rust
use thiserror::Error;

/// Session registry specific errors
#[derive(Error, Debug)]
pub enum RegistryError {
    /// Registry is at capacity
    #[error("Registry full: cannot add session (max: {max})")]
    Full { max: usize },

    /// Session not found for update/removal
    #[error("Session not found: {session_id}")]
    NotFound { session_id: SessionId },

    /// Duplicate session registration attempted
    #[error("Session already exists: {session_id}")]
    AlreadyExists { session_id: SessionId },

    /// Internal actor communication failed
    #[error("Registry actor not responding")]
    ActorUnresponsive,
}
```

---

## Shell Script Exit Codes

Shell scripts used by Claude Code (status line and hooks) follow a strict exit code convention:

```bash
#!/bin/bash
# Exit code semantics for Agent Tmux Manager shell scripts
#
# EXIT CODES:
#   0 = Success (or graceful degradation - ALWAYS USE FOR CLAUDE CODE)
#   1 = Daemon unavailable (silent failure, used internally)
#   2 = Invalid message format (parse error)
#   3 = Timeout exceeded
#   4 = Configuration error
#   5 = Internal error
#
# CRITICAL RULE: Scripts invoked by Claude Code MUST always exit 0
# to avoid breaking Claude Code operation. Log errors to file instead.
```

### Exit Code Constants (for internal use)

```bash
# Exit codes (internal use only - always exit 0 for Claude Code)
readonly EXIT_SUCCESS=0
readonly EXIT_DAEMON_UNAVAILABLE=1
readonly EXIT_INVALID_MESSAGE=2
readonly EXIT_TIMEOUT=3
readonly EXIT_CONFIG_ERROR=4
readonly EXIT_INTERNAL_ERROR=5

# For Claude Code integration, wrap all exits:
safe_exit() {
    local code=$1
    local message=$2

    if [ $code -ne 0 ]; then
        echo "$(date -Iseconds): Exit $code - $message" >> /tmp/atm-client.log
    fi

    # ALWAYS exit 0 for Claude Code
    exit 0
}
```

---

## Retry Policies

### Bash Scripts (Status Line and Hooks)

Bash scripts use a **no-retry, fail-fast** approach to avoid blocking Claude Code:

```bash
#!/bin/bash
# atm-status.sh - Status line handler for Claude Code
#
# Design: Non-blocking, no retries, silent failure
# If daemon is unavailable, we simply don't report - better than blocking Claude Code

set -euo pipefail

readonly SOCKET="/tmp/atm.sock"
readonly TIMEOUT_SECONDS=0.1
readonly LOG_FILE="/tmp/atm-client.log"
readonly MAX_LOG_SIZE=1048576  # 1MB

# Rotate log if too large
rotate_log_if_needed() {
    if [ -f "$LOG_FILE" ] && [ "$(stat -f%z "$LOG_FILE" 2>/dev/null || stat -c%s "$LOG_FILE" 2>/dev/null)" -gt "$MAX_LOG_SIZE" ]; then
        mv "$LOG_FILE" "${LOG_FILE}.old"
    fi
}

log_error() {
    rotate_log_if_needed
    echo "$(date -Iseconds): $1" >> "$LOG_FILE"
}

# Quick socket existence check (fastest possible)
if [ ! -S "$SOCKET" ]; then
    # Daemon not running - silently exit
    # Don't even log this - it's a normal condition during startup
    exit 0
fi

# Read input from Claude Code (single read, not while loop)
input=$(cat)

if [ -z "$input" ]; then
    exit 0
fi

# Construct message with minimal processing
session_id="${CLAUDE_SESSION_ID:-${ATM_SESSION_ID:-unknown}}"
message=$(jq -c --arg sid "$session_id" '. + {session_id: $sid, type: "status_update"}' <<< "$input" 2>/dev/null) || {
    log_error "Failed to construct JSON message"
    exit 0
}

# Send with timeout - no retries
# Using timeout command for portability
if ! timeout "$TIMEOUT_SECONDS" bash -c "echo '$message' | nc -U '$SOCKET'" 2>/dev/null; then
    log_error "Failed to send to daemon (timeout or connection refused)"
    # Exit 0 anyway - don't break Claude Code
fi

exit 0
```

### Hook Script Pattern

```bash
#!/bin/bash
# atm-hook.sh - Hook handler for Claude Code
#
# Invoked for PreToolUse, PostToolUse events
# Must complete quickly and never block

set -euo pipefail

readonly SOCKET="/tmp/atm.sock"
readonly TIMEOUT_SECONDS=0.1
readonly LOG_FILE="/tmp/atm-hooks.log"

log_error() {
    echo "$(date -Iseconds): $1" >> "$LOG_FILE" 2>/dev/null || true
}

# Fast-path: Check socket exists
[ -S "$SOCKET" ] || exit 0

# Read hook event (single read)
input=$(cat)
[ -n "$input" ] || exit 0

# Parse hook event type for logging
hook_event=$(echo "$input" | jq -r '.hook_event_name // "unknown"' 2>/dev/null) || hook_event="unknown"

# Forward to daemon with timeout
session_id="${CLAUDE_SESSION_ID:-${ATM_SESSION_ID:-unknown}}"
message=$(jq -c --arg sid "$session_id" '. + {session_id: $sid}' <<< "$input" 2>/dev/null) || {
    log_error "[$hook_event] Failed to construct message"
    exit 0
}

timeout "$TIMEOUT_SECONDS" bash -c "echo '$message' | nc -U '$SOCKET'" 2>/dev/null || {
    log_error "[$hook_event] Failed to send to daemon"
}

exit 0
```

### TUI Daemon Connection Retry

The TUI uses exponential backoff for daemon connection:

```rust
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::sleep;
use tracing::{info, warn, error};

/// Configuration for connection retry behavior
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Initial delay before first retry
    pub initial_delay: Duration,
    /// Maximum delay between retries (cap for exponential backoff)
    pub max_delay: Duration,
    /// Multiplier for exponential backoff
    pub multiplier: f64,
    /// Maximum number of retry attempts (None = infinite)
    pub max_attempts: Option<u32>,
    /// Jitter factor (0.0 = no jitter, 1.0 = up to 100% random jitter)
    pub jitter: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            multiplier: 2.0,
            max_attempts: None,  // Retry forever by default
            jitter: 0.1,         // 10% jitter to prevent thundering herd
        }
    }
}

impl RetryConfig {
    /// Create a config for aggressive reconnection (TUI use case)
    pub fn aggressive() -> Self {
        Self {
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(5),
            multiplier: 1.5,
            max_attempts: None,
            jitter: 0.2,
        }
    }

    /// Calculate delay for given attempt number
    fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let base_delay = self.initial_delay.as_secs_f64()
            * self.multiplier.powi(attempt as i32);
        let capped_delay = base_delay.min(self.max_delay.as_secs_f64());

        // Add jitter
        let jitter_range = capped_delay * self.jitter;
        let jitter = (rand::random::<f64>() - 0.5) * 2.0 * jitter_range;
        let final_delay = (capped_delay + jitter).max(0.0);

        Duration::from_secs_f64(final_delay)
    }
}

/// Client for connecting to the ATM daemon
pub struct DaemonClient {
    socket_path: PathBuf,
    retry_config: RetryConfig,
}

impl DaemonClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            retry_config: RetryConfig::default(),
        }
    }

    pub fn with_retry_config(mut self, config: RetryConfig) -> Self {
        self.retry_config = config;
        self
    }

    /// Attempt to connect with retry logic
    ///
    /// Returns the connected stream or an error if max_attempts exceeded.
    /// Includes a total operation timeout to prevent infinite retries.
    pub async fn connect_with_retry(&self) -> Result<UnixStream, TuiError> {
        use tokio::time::timeout;

        // Total timeout for the entire retry operation
        let total_timeout = Duration::from_secs(60);

        timeout(total_timeout, self.connect_with_retry_inner())
            .await
            .map_err(|_| TuiError::ConnectionTimeout { timeout_secs: 60 })?
    }

    /// Inner retry logic without total timeout
    async fn connect_with_retry_inner(&self) -> Result<UnixStream, TuiError> {
        let mut attempt: u32 = 0;

        loop {
            match UnixStream::connect(&self.socket_path).await {
                Ok(stream) => {
                    if attempt > 0 {
                        info!(
                            attempts = attempt + 1,
                            "Connected to daemon after retries"
                        );
                    } else {
                        info!("Connected to daemon");
                    }
                    return Ok(stream);
                }
                Err(e) => {
                    // Check if we've exceeded max attempts
                    if let Some(max) = self.retry_config.max_attempts {
                        if attempt >= max {
                            error!(
                                attempts = attempt,
                                max_attempts = max,
                                error = %e,
                                "Failed to connect to daemon after max attempts"
                            );
                            return Err(TuiError::ConnectionTimeout {
                                timeout_secs: self.retry_config.max_delay.as_secs()
                                    * max as u64,
                            });
                        }
                    }

                    let delay = self.retry_config.delay_for_attempt(attempt);

                    warn!(
                        attempt = attempt + 1,
                        delay_ms = delay.as_millis(),
                        error = %e,
                        socket_path = %self.socket_path.display(),
                        "Failed to connect to daemon, retrying"
                    );

                    sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    /// Try to connect once without retry (for status checks)
    pub async fn try_connect(&self) -> Result<UnixStream, TuiError> {
        UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::ConnectionRefused
                {
                    TuiError::DaemonNotRunning {
                        socket_path: self.socket_path.clone(),
                    }
                } else {
                    TuiError::Io(e)
                }
            })
    }
}

// Mock rand for example (in real code, use rand crate)
mod rand {
    pub fn random<T: Default>() -> T { T::default() }
}

// Placeholder for TuiError (defined above)
use super::TuiError;
```

---

## Daemon Error Recovery

The daemon is designed to be resilient to individual connection and message failures:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn, instrument};

/// Unique identifier for connected clients
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientId(pub u64);

/// Information about a connected client
pub struct ClientInfo {
    pub id: ClientId,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub client_type: ClientType,
    pub protocol_version: String,
}

#[derive(Debug, Clone)]
pub enum ClientType {
    Session,  // Claude Code session reporter
    Tui,      // TUI viewer
}

/// Handle an individual client connection
///
/// This function is designed to:
/// 1. Never panic (all errors are caught and logged)
/// 2. Not affect other connections if this one fails
/// 3. Clean up resources properly on disconnect
#[instrument(skip(stream, registry), fields(client_addr))]
pub async fn handle_connection(
    stream: UnixStream,
    registry: RegistryHandle,
    active_clients: Arc<Mutex<HashMap<ClientId, ClientInfo>>>,
) {
    let client_id = ClientId(rand::random());

    // Get peer address for logging (may not be available on Unix sockets)
    let client_addr = stream
        .peer_addr()
        .map(|a| format!("{:?}", a))
        .unwrap_or_else(|_| "unknown".to_string());

    tracing::Span::current().record("client_addr", &client_addr);
    info!(client_id = ?client_id, "New client connection");

    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // Track if handshake completed
    let mut handshake_complete = false;
    let mut client_info: Option<ClientInfo> = None;

    // Timeout for reading individual messages
    const MESSAGE_READ_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

    // Message processing loop
    loop {
        // Use timeout on read operations to detect stale connections
        let line_result = tokio::time::timeout(
            MESSAGE_READ_TIMEOUT,
            lines.next_line()
        ).await;

        let line_result = match line_result {
            Ok(result) => result,
            Err(_) => {
                // Read timeout - connection may be stale
                warn!(client_id = ?client_id, "Message read timeout, closing connection");
                break;
            }
        };

        match line_result {
            Ok(Some(line)) => {
                // Skip empty lines
                if line.trim().is_empty() {
                    continue;
                }

                // Check message size
                if line.len() > MAX_MESSAGE_SIZE {
                    error!(
                        client_id = ?client_id,
                        size = line.len(),
                        max = MAX_MESSAGE_SIZE,
                        "Message too large, closing connection"
                    );
                    break;
                }

                // Parse and handle message
                match serde_json::from_str::<ClientMessage>(&line) {
                    Ok(msg) => {
                        // Ensure handshake for non-register messages
                        if !handshake_complete && !matches!(msg, ClientMessage::Register { .. }) {
                            warn!(
                                client_id = ?client_id,
                                message_type = ?msg.message_type(),
                                "Message received before handshake"
                            );
                            // Could send error response here
                            continue;
                        }

                        // Process the message
                        match handle_message(&client_id, msg, &registry).await {
                            Ok(response) => {
                                if !handshake_complete {
                                    handshake_complete = true;
                                    // Record client info
                                    // ...
                                }
                                // Send response if any
                                debug!(client_id = ?client_id, "Message handled successfully");
                            }
                            Err(e) => {
                                // Log error but keep connection alive for recoverable errors
                                match &e {
                                    DaemonError::SessionNotFound(_) => {
                                        warn!(
                                            client_id = ?client_id,
                                            error = %e,
                                            "Recoverable error, continuing"
                                        );
                                    }
                                    DaemonError::RegistryFull { .. } => {
                                        error!(
                                            client_id = ?client_id,
                                            error = %e,
                                            "Registry full, rejecting registration"
                                        );
                                        // Could send error response
                                    }
                                    _ => {
                                        error!(
                                            client_id = ?client_id,
                                            error = %e,
                                            "Error handling message"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // JSON parse error - log but continue
                        warn!(
                            client_id = ?client_id,
                            error = %e,
                            line_preview = &line[..line.len().min(100)],
                            "Failed to parse message, skipping"
                        );
                        // Don't close connection for parse errors
                    }
                }
            }
            Ok(None) => {
                // Clean EOF - client disconnected gracefully
                info!(client_id = ?client_id, "Client disconnected");
                break;
            }
            Err(e) => {
                // I/O error - connection lost
                warn!(
                    client_id = ?client_id,
                    error = %e,
                    "Connection error, closing"
                );
                break;
            }
        }
    }

    // Cleanup: Remove client from active clients
    {
        let mut clients = active_clients.lock().await;
        clients.remove(&client_id);
        info!(
            client_id = ?client_id,
            remaining_clients = clients.len(),
            "Client removed from active list"
        );
    }
}

/// Handle a parsed client message
async fn handle_message(
    client_id: &ClientId,
    msg: ClientMessage,
    registry: &RegistryHandle,
) -> Result<Option<DaemonMessage>, DaemonError> {
    match msg {
        ClientMessage::Register { session_id, agent_type, pid, cwd } => {
            let session = SessionDomain::new(
                SessionId(session_id),
                agent_type,
                pid,
                cwd,
            );
            registry.register(session).await?;
            Ok(Some(DaemonMessage::Registered { success: true }))
        }
        ClientMessage::StatusUpdate { session_id, context, cost } => {
            registry.update_context(SessionId(session_id), context).await?;
            Ok(None)  // No response for status updates
        }
        ClientMessage::Unregister { session_id } => {
            registry.unregister(SessionId(session_id)).await?;
            Ok(Some(DaemonMessage::Unregistered { success: true }))
        }
        ClientMessage::GetSessions => {
            let sessions = registry.get_all_sessions().await;
            Ok(Some(DaemonMessage::Sessions { sessions }))
        }
    }
}

const MAX_MESSAGE_SIZE: usize = 1_048_576;  // 1MB

// Placeholder types (defined elsewhere)
pub struct RegistryHandle;
pub struct SessionDomain;
pub enum ClientMessage {
    Register { session_id: String, agent_type: String, pid: u32, cwd: String },
    StatusUpdate { session_id: String, context: (), cost: () },
    Unregister { session_id: String },
    GetSessions,
}
impl ClientMessage {
    fn message_type(&self) -> &'static str {
        match self {
            Self::Register { .. } => "register",
            Self::StatusUpdate { .. } => "status_update",
            Self::Unregister { .. } => "unregister",
            Self::GetSessions => "get_sessions",
        }
    }
}
pub enum DaemonMessage {
    Registered { success: bool },
    Unregistered { success: bool },
    Sessions { sessions: Vec<()> },
}

impl RegistryHandle {
    async fn register(&self, _: SessionDomain) -> Result<(), DaemonError> { Ok(()) }
    async fn update_context(&self, _: SessionId, _: ()) -> Result<(), DaemonError> { Ok(()) }
    async fn unregister(&self, _: SessionId) -> Result<(), DaemonError> { Ok(()) }
    async fn get_all_sessions(&self) -> Vec<()> { vec![] }
}

impl SessionDomain {
    fn new(_: SessionId, _: String, _: u32, _: String) -> Self { Self }
}

// Mock rand for example
mod rand {
    pub fn random<T: Default>() -> T { T::default() }
}
```

---

## Graceful Degradation Strategies

### Bash Scripts: Daemon Unavailable

When the daemon is not running, scripts silently continue without reporting:

```bash
#!/bin/bash
# Pattern: Check before connect, fail silently

readonly SOCKET="/tmp/atm.sock"

# Strategy 1: Quick existence check (recommended)
if [ ! -S "$SOCKET" ]; then
    # Don't log, don't error - just exit cleanly
    exit 0
fi

# Strategy 2: Connection test with immediate timeout
if ! timeout 0.05 bash -c "true >/dev/tcp/localhost/1" 2>/dev/null; then
    # Using /dev/tcp as a fast connectivity test pattern
    :
fi

# Strategy 3: Use nc with -z for zero-I/O connection test
if ! nc -zU "$SOCKET" 2>/dev/null; then
    exit 0
fi
```

### TUI: Daemon Disconnected

The TUI shows a reconnection banner while attempting to reconnect:

```rust
use chrono::{DateTime, Utc};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};

/// Application connection state
#[derive(Debug, Clone)]
pub enum ConnectionState {
    /// Connected and receiving updates
    Connected,
    /// Attempting initial connection
    Connecting { since: DateTime<Utc>, attempts: u32 },
    /// Lost connection, attempting to reconnect
    Reconnecting { since: DateTime<Utc>, attempts: u32 },
    /// Failed to connect after max attempts
    Failed { reason: String },
}

impl ConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionState::Connected)
    }

    pub fn disconnected_duration(&self) -> Option<chrono::Duration> {
        match self {
            ConnectionState::Reconnecting { since, .. } |
            ConnectionState::Connecting { since, .. } => {
                Some(Utc::now().signed_duration_since(*since))
            }
            _ => None,
        }
    }
}

/// Render connection status banner
pub fn render_connection_banner(
    state: &ConnectionState,
    area: Rect,
    buf: &mut Buffer,
) {
    match state {
        ConnectionState::Connected => {
            // No banner needed when connected
        }
        ConnectionState::Connecting { attempts, .. } => {
            let text = format!(
                " Connecting to daemon... (attempt {}) ",
                attempts
            );
            let banner = Paragraph::new(text)
                .style(Style::default()
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD));
            banner.render(area, buf);
        }
        ConnectionState::Reconnecting { since, attempts } => {
            let duration = Utc::now().signed_duration_since(*since);
            let text = format!(
                " Daemon disconnected ({} seconds ago) - Reconnecting (attempt {})... ",
                duration.num_seconds(),
                attempts
            );
            let banner = Paragraph::new(text)
                .style(Style::default()
                    .bg(Color::Red)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD));
            banner.render(area, buf);
        }
        ConnectionState::Failed { reason } => {
            let text = format!(
                " Connection failed: {} - Press 'r' to retry or 'q' to quit ",
                reason
            );
            let banner = Paragraph::new(text)
                .style(Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::White));
            banner.render(area, buf);
        }
    }
}

/// Handle reconnection in application loop
pub async fn reconnection_loop(
    state: &mut ConnectionState,
    client: &DaemonClient,
) -> Option<UnixStream> {
    match state {
        ConnectionState::Reconnecting { attempts, .. } |
        ConnectionState::Connecting { attempts, .. } => {
            match client.try_connect().await {
                Ok(stream) => {
                    *state = ConnectionState::Connected;
                    Some(stream)
                }
                Err(_) => {
                    *attempts += 1;
                    None
                }
            }
        }
        _ => None,
    }
}

// Placeholder types
use tokio::net::UnixStream;
struct DaemonClient {
    socket_path: std::path::PathBuf,
}
impl DaemonClient {
    async fn try_connect(&self) -> Result<UnixStream, TuiError> {
        // Stub implementation - returns appropriate error instead of panicking
        Err(TuiError::DaemonNotRunning {
            socket_path: self.socket_path.clone(),
        })
    }
}
```

### Cooperative Shutdown with CancellationToken

Use `tokio_util::sync::CancellationToken` for graceful shutdown of background tasks:

```rust
use tokio_util::sync::CancellationToken;
use tokio::time::{interval, Duration};

/// Spawn a background task with cancellation support
pub fn spawn_cleanup_task(
    handle: RegistryHandle,
    cancel_token: CancellationToken,
) {
    tokio::spawn(async move {
        let mut tick_interval = interval(Duration::from_secs(30));

        loop {
            tokio::select! {
                // Check for cancellation first (biased)
                biased;

                _ = cancel_token.cancelled() => {
                    tracing::info!("Cleanup task received shutdown signal");
                    break;
                }

                _ = tick_interval.tick() => {
                    match handle.cleanup_stale().await {
                        Ok(removed) if removed > 0 => {
                            tracing::info!(removed_count = removed, "Cleaned up stale sessions");
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = ?e, "Cleanup failed");
                        }
                    }
                }
            }
        }

        tracing::info!("Cleanup task shut down gracefully");
    });
}

/// Daemon with coordinated shutdown
pub struct Daemon {
    cancel_token: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Daemon {
    pub fn new() -> Self {
        Self {
            cancel_token: CancellationToken::new(),
            tasks: Vec::new(),
        }
    }

    /// Spawn a task that respects the cancellation token
    pub fn spawn_task<F>(&mut self, name: &'static str, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let cancel = self.cancel_token.clone();
        self.tasks.push(tokio::spawn(async move {
            tokio::select! {
                _ = future => {
                    tracing::debug!(task = name, "Task completed");
                }
                _ = cancel.cancelled() => {
                    tracing::debug!(task = name, "Task cancelled");
                }
            }
        }));
    }

    /// Initiate graceful shutdown
    pub async fn shutdown(self) {
        tracing::info!("Initiating daemon shutdown");

        // Signal all tasks to stop
        self.cancel_token.cancel();

        // Wait for tasks with timeout
        let shutdown_timeout = Duration::from_secs(5);
        for task in self.tasks {
            let _ = tokio::time::timeout(shutdown_timeout, task).await;
        }

        tracing::info!("Daemon shutdown complete");
    }
}
```

**Key points:**
- Use `CancellationToken` instead of raw channels for shutdown signaling
- Always check cancellation with `biased;` to prioritize shutdown
- Give tasks a timeout to complete during shutdown
- Log shutdown events for debugging

---

### Daemon: Resource Exhaustion

When the daemon reaches capacity, it rejects new connections gracefully:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{info, warn};

const MAX_CLIENTS: usize = 10;
const MAX_SESSIONS: usize = 100;

pub struct DaemonServer {
    listener: UnixListener,
    active_clients: Arc<Mutex<HashMap<ClientId, ClientInfo>>>,
    registry: RegistryHandle,
}

impl DaemonServer {
    /// Accept loop with capacity checking
    pub async fn accept_loop(&self) {
        loop {
            match self.listener.accept().await {
                Ok((stream, _addr)) => {
                    if let Err(e) = self.try_accept_client(stream).await {
                        warn!(error = %e, "Failed to accept client");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Accept error");
                    // Brief sleep to prevent tight loop on persistent errors
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }
        }
    }

    async fn try_accept_client(&self, mut stream: UnixStream) -> Result<(), DaemonError> {
        // Check client capacity
        let client_count = self.active_clients.lock().await.len();

        if client_count >= MAX_CLIENTS {
            warn!(
                current = client_count,
                max = MAX_CLIENTS,
                "Client limit reached, rejecting connection"
            );

            // Send rejection message
            let error_msg = serde_json::json!({
                "type": "error",
                "code": "server_at_capacity",
                "message": format!("Server at capacity ({}/{}). Please try again later.", client_count, MAX_CLIENTS),
                "retry_after_seconds": 5
            });

            // serde_json::to_string only fails on non-string keys or invalid values
            // Our json! literal is always valid, so this is safe
            if let Ok(msg) = serde_json::to_string(&error_msg) {
                let _ = stream.write_all(msg.as_bytes()).await;
                let _ = stream.write_all(b"\n").await;
            }
            let _ = stream.shutdown().await;

            return Err(DaemonError::TooManyClients { max: MAX_CLIENTS });
        }

        // Accept the client
        info!(
            current_clients = client_count + 1,
            "Accepted new client connection"
        );

        // Spawn handler task
        let registry = self.registry.clone();
        let active_clients = self.active_clients.clone();
        tokio::spawn(async move {
            handle_connection(stream, registry, active_clients).await;
        });

        Ok(())
    }
}

/// When registry is full, attempt cleanup before rejecting
pub async fn register_with_cleanup(
    registry: &RegistryHandle,
    session: SessionDomain,
) -> Result<(), DaemonError> {
    // First attempt
    match registry.register(session.clone()).await {
        Ok(()) => return Ok(()),
        Err(DaemonError::RegistryFull { max, current }) => {
            info!(
                max = max,
                current = current,
                "Registry full, attempting cleanup"
            );

            // Trigger cleanup of stale sessions
            let cleaned = registry.cleanup_stale_sessions().await;
            info!(cleaned_sessions = cleaned, "Cleanup completed");

            // Retry after cleanup
            registry.register(session).await
        }
        Err(e) => Err(e),
    }
}

impl RegistryHandle {
    fn clone(&self) -> Self { Self }
    async fn register(&self, _: SessionDomain) -> Result<(), DaemonError> { Ok(()) }
    async fn cleanup_stale_sessions(&self) -> usize { 0 }
}
```

---

## Logging Strategy

### Structured Logging with tracing

All Rust code uses the `tracing` crate for structured, contextual logging:

```rust
use tracing::{debug, error, info, instrument, warn, Level};
use tracing_subscriber::{
    fmt::{self, format::FmtSpan},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
};

/// Initialize logging for the daemon
pub fn init_daemon_logging() -> Result<(), Box<dyn std::error::Error>> {
    let log_dir = dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .ok_or("Could not determine state directory")?
        .join("atm");

    std::fs::create_dir_all(&log_dir)?;

    // File appender with daily rotation
    // Note: This performs file I/O during initialization, which is acceptable
    // because it runs during daemon startup before the async runtime is fully active.
    // If called from an async context, wrap in spawn_blocking.
    let file_appender = tracing_appender::rolling::daily(&log_dir, "atm.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Build subscriber with multiple layers
    tracing_subscriber::registry()
        // JSON format for file (machine-readable)
        .with(
            fmt::layer()
                .json()
                .with_writer(non_blocking)
                .with_span_events(FmtSpan::CLOSE)
                .with_current_span(true)
        )
        // Pretty format for stderr (human-readable, only in debug)
        .with(
            fmt::layer()
                .pretty()
                .with_writer(std::io::stderr)
                .with_filter(EnvFilter::from_default_env())
        )
        .init();

    Ok(())
}

/// Initialize logging for the TUI (stderr only)
pub fn init_tui_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .compact()
        .init();
}

/// Example: Instrumented function with structured fields
#[instrument(
    skip(registry),
    fields(
        session_id = %session.id,
        agent_type = ?session.agent_type
    )
)]
pub async fn handle_register(
    session: SessionDomain,
    registry: &RegistryHandle,
) -> Result<(), DaemonError> {
    debug!("Starting session registration");

    match registry.register(session.clone()).await {
        Ok(()) => {
            info!(
                session_id = %session.id,
                agent_type = ?session.agent_type,
                "Session registered successfully"
            );
            Ok(())
        }
        Err(e) => {
            error!(
                session_id = %session.id,
                error = %e,
                error_debug = ?e,
                "Failed to register session"
            );
            Err(e)
        }
    }
}

/// Example: Logging with dynamic fields
pub fn log_connection_event(client_id: &ClientId, event: &str, details: &str) {
    info!(
        client_id = ?client_id,
        event = event,
        details = details,
        timestamp = %chrono::Utc::now(),
        "Connection event"
    );
}

/// Example: Conditional logging based on severity
pub fn log_message_handling_result(
    session_id: &SessionId,
    message_type: &str,
    result: &Result<(), DaemonError>,
) {
    match result {
        Ok(()) => {
            debug!(
                session_id = %session_id,
                message_type = message_type,
                "Message handled"
            );
        }
        Err(DaemonError::SessionNotFound(_)) => {
            warn!(
                session_id = %session_id,
                message_type = message_type,
                "Message for unknown session (may have expired)"
            );
        }
        Err(e) => {
            error!(
                session_id = %session_id,
                message_type = message_type,
                error = %e,
                "Failed to handle message"
            );
        }
    }
}

// Placeholder for dirs crate
mod dirs {
    use std::path::PathBuf;
    pub fn state_dir() -> Option<PathBuf> { None }
    pub fn home_dir() -> Option<PathBuf> { Some(PathBuf::from("/home/user")) }
}
```

### Log Levels

| Level | Usage | Examples |
|-------|-------|----------|
| **ERROR** | Unexpected failures requiring investigation | Registry corruption, unhandled exceptions, I/O failures |
| **WARN** | Degraded operation, recoverable issues | Disconnections, retry attempts, limits approaching |
| **INFO** | Normal operation milestones | Sessions registered, clients connected, daemon started |
| **DEBUG** | Detailed flow for troubleshooting | Messages received, state transitions, timing |
| **TRACE** | Very verbose, development only | Full message contents, internal state dumps |

### Log Destinations

| Component | Destination | Format | Rotation |
|-----------|-------------|--------|----------|
| **Daemon** | `~/.local/state/atm/atm.log` | JSON | Daily, keep 7 days |
| **TUI** | stderr | Compact text | N/A |
| **Bash scripts** | `/tmp/atm-client.log` | Text with timestamp | Manual, max 1MB |

---

## User-Facing Error Messages

### TUI Error Display

```rust
use crate::errors::TuiError;

/// Convert internal errors to user-friendly messages
pub fn user_message(error: &TuiError) -> UserMessage {
    match error {
        TuiError::DaemonDisconnected => UserMessage {
            title: "Disconnected".to_string(),
            body: "Lost connection to the ATM daemon. Attempting to reconnect...".to_string(),
            suggestion: None,
            recoverable: true,
        },

        TuiError::ConnectionTimeout { timeout_secs } => UserMessage {
            title: "Connection Timeout".to_string(),
            body: format!(
                "Could not connect to daemon after {} seconds.",
                timeout_secs
            ),
            suggestion: Some("Is the daemon running? Try: atmd start".to_string()),
            recoverable: true,
        },

        TuiError::DaemonNotRunning { socket_path } => UserMessage {
            title: "Daemon Not Running".to_string(),
            body: format!(
                "The ATM daemon is not running.\nSocket not found: {:?}",
                socket_path
            ),
            suggestion: Some("Start the daemon with: atmd start".to_string()),
            recoverable: false,
        },

        TuiError::RenderError { reason } => UserMessage {
            title: "Display Error".to_string(),
            body: format!("Failed to render display: {}", reason),
            suggestion: Some("Try resizing your terminal window.".to_string()),
            recoverable: true,
        },

        TuiError::InvalidTerminalState { reason } => UserMessage {
            title: "Terminal Error".to_string(),
            body: format!("Invalid terminal state: {}", reason),
            suggestion: Some("Please restart the application.".to_string()),
            recoverable: false,
        },

        TuiError::VersionMismatch { client_version, daemon_version } => UserMessage {
            title: "Version Mismatch".to_string(),
            body: format!(
                "Protocol version mismatch.\nClient: {}\nDaemon: {}",
                client_version, daemon_version
            ),
            suggestion: Some(
                "Please update Agent Tmux Manager:\n  cargo install atm --force".to_string()
            ),
            recoverable: false,
        },

        TuiError::Io(e) => UserMessage {
            title: "I/O Error".to_string(),
            body: format!("An I/O error occurred: {}", e),
            suggestion: match e.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    Some("Check file permissions for the socket.".to_string())
                }
                std::io::ErrorKind::NotFound => {
                    Some("Is the daemon running?".to_string())
                }
                _ => None,
            },
            recoverable: false,
        },

        TuiError::ParseError(e) => UserMessage {
            title: "Protocol Error".to_string(),
            body: format!("Failed to parse daemon message: {}", e),
            suggestion: Some(
                "This may indicate a version mismatch. Try updating both components.".to_string()
            ),
            recoverable: true,
        },

        TuiError::TerminalInit(reason) => UserMessage {
            title: "Startup Failed".to_string(),
            body: format!("Could not initialize terminal: {}", reason),
            suggestion: Some(
                "Ensure you're running in a supported terminal emulator.".to_string()
            ),
            recoverable: false,
        },

        TuiError::TerminalCleanup(reason) => UserMessage {
            title: "Cleanup Warning".to_string(),
            body: format!("Terminal may be in an inconsistent state: {}", reason),
            suggestion: Some("Run 'reset' command if your terminal looks broken.".to_string()),
            recoverable: false,
        },
    }
}

/// Structured user-facing message
pub struct UserMessage {
    pub title: String,
    pub body: String,
    pub suggestion: Option<String>,
    pub recoverable: bool,
}

impl UserMessage {
    /// Format for display in TUI
    pub fn format_for_display(&self) -> String {
        let mut result = format!("{}\n\n{}", self.title, self.body);

        if let Some(suggestion) = &self.suggestion {
            result.push_str("\n\n");
            result.push_str(suggestion);
        }

        if self.recoverable {
            result.push_str("\n\nPress any key to continue...");
        } else {
            result.push_str("\n\nPress 'q' to quit.");
        }

        result
    }
}
```

---

## Testing Error Handling

### Unit Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Test that registry returns appropriate error when full
    #[tokio::test]
    async fn test_registry_full_error() {
        let registry = TestRegistry::new(max_sessions: 2);

        // Fill the registry
        let session1 = create_test_session("session-1");
        let session2 = create_test_session("session-2");

        assert!(registry.register(session1).await.is_ok());
        assert!(registry.register(session2).await.is_ok());

        // Third registration should fail
        let session3 = create_test_session("session-3");
        let result = registry.register(session3).await;

        assert!(matches!(
            result,
            Err(DaemonError::RegistryFull { max: 2, .. })
        ));
    }

    /// Test that session not found is handled gracefully
    #[tokio::test]
    async fn test_update_nonexistent_session() {
        let registry = TestRegistry::new(max_sessions: 10);

        let result = registry.update_context(
            SessionId("nonexistent".to_string()),
            ContextUsage::default(),
        ).await;

        assert!(matches!(
            result,
            Err(DaemonError::SessionNotFound(_))
        ));
    }

    /// Test error conversion and display
    #[test]
    fn test_error_display() {
        let error = DaemonError::RegistryFull { max: 100, current: 100 };
        let display = format!("{}", error);
        assert!(display.contains("100"));
        assert!(display.contains("full"));
    }

    /// Test protocol error chain
    #[test]
    fn test_protocol_error_chain() {
        let protocol_err = ProtocolError::MissingField {
            field: "session_id".to_string(),
            message_type: "register".to_string(),
        };

        let daemon_err: DaemonError = protocol_err.into();

        assert!(matches!(daemon_err, DaemonError::Protocol(_)));
        let display = format!("{}", daemon_err);
        assert!(display.contains("session_id"));
    }

    // Helper functions for tests
    fn create_test_session(id: &str) -> SessionDomain {
        SessionDomain {
            id: SessionId(id.to_string()),
            // ... other fields
        }
    }

    struct TestRegistry {
        max_sessions: usize,
        sessions: std::collections::HashMap<SessionId, SessionDomain>,
    }

    impl TestRegistry {
        fn new(max_sessions: usize) -> Self {
            Self {
                max_sessions,
                sessions: std::collections::HashMap::new(),
            }
        }

        async fn register(&mut self, session: SessionDomain) -> Result<(), DaemonError> {
            if self.sessions.len() >= self.max_sessions {
                return Err(DaemonError::RegistryFull {
                    max: self.max_sessions,
                    current: self.sessions.len(),
                });
            }
            self.sessions.insert(session.id.clone(), session);
            Ok(())
        }

        async fn update_context(
            &mut self,
            session_id: SessionId,
            _context: ContextUsage,
        ) -> Result<(), DaemonError> {
            if !self.sessions.contains_key(&session_id) {
                return Err(DaemonError::SessionNotFound(session_id));
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct ContextUsage;
}
```

### Integration Tests

```rust
#[cfg(test)]
mod integration_tests {
    use super::*;
    use tokio::net::UnixStream;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Test that daemon survives malformed JSON
    #[tokio::test]
    async fn test_daemon_survives_malformed_json() {
        // Start test daemon
        let daemon = TestDaemon::start().await;

        // Connect as client
        let mut stream = UnixStream::connect(&daemon.socket_path).await.unwrap();

        // Send malformed JSON
        stream.write_all(b"{ this is not valid json }\n").await.unwrap();

        // Small delay to let daemon process
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Send valid message - daemon should still respond
        let valid_msg = r#"{"type": "get_sessions"}"#;
        stream.write_all(valid_msg.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();

        // Read response
        let mut buf = vec![0u8; 1024];
        let n = stream.read(&mut buf).await.unwrap();

        // Should get valid response
        let response: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();
        assert!(response.get("sessions").is_some());

        daemon.stop().await;
    }

    /// Test reconnection after daemon restart
    #[tokio::test]
    async fn test_tui_reconnects_after_daemon_restart() {
        let daemon = TestDaemon::start().await;
        let socket_path = daemon.socket_path.clone();

        let client = DaemonClient::new(socket_path.clone())
            .with_retry_config(RetryConfig {
                initial_delay: Duration::from_millis(10),
                max_delay: Duration::from_millis(100),
                max_attempts: Some(10),
                ..Default::default()
            });

        // Connect initially
        let _stream = client.connect_with_retry().await.unwrap();

        // Stop daemon
        daemon.stop().await;

        // Start new daemon
        let _new_daemon = TestDaemon::start_at(&socket_path).await;

        // Should reconnect
        let result = client.connect_with_retry().await;
        assert!(result.is_ok());
    }

    /// Test that full registry rejects new sessions
    #[tokio::test]
    async fn test_registry_full_rejection() {
        let daemon = TestDaemon::start_with_config(DaemonConfig {
            max_sessions: 2,
            ..Default::default()
        }).await;

        let mut stream = UnixStream::connect(&daemon.socket_path).await.unwrap();

        // Register max sessions
        for i in 0..2 {
            let msg = format!(r#"{{"type": "register", "session_id": "session-{}"}}"#, i);
            stream.write_all(msg.as_bytes()).await.unwrap();
            stream.write_all(b"\n").await.unwrap();
        }

        // Third should fail
        let msg = r#"{"type": "register", "session_id": "session-overflow"}"#;
        stream.write_all(msg.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();

        // Read response
        let mut buf = vec![0u8; 1024];
        let n = stream.read(&mut buf).await.unwrap();
        let response: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();

        assert_eq!(response.get("type").unwrap(), "error");
        assert!(response.get("message").unwrap().as_str().unwrap().contains("full"));

        daemon.stop().await;
    }

    // Test helper structures
    struct TestDaemon {
        socket_path: std::path::PathBuf,
        handle: tokio::task::JoinHandle<()>,
    }

    impl TestDaemon {
        async fn start() -> Self {
            let socket_path = std::env::temp_dir().join(format!(
                "atm-test-{}.sock",
                std::process::id()
            ));
            Self::start_at(&socket_path).await
        }

        async fn start_at(socket_path: &std::path::Path) -> Self {
            // Stub implementation - in real code, this would start the daemon
            // For tests, this creates a mock daemon at the given path
            Self {
                socket_path: socket_path.to_path_buf(),
                handle: tokio::spawn(async {}),
            }
        }

        async fn start_with_config(_config: DaemonConfig) -> Self {
            // Stub implementation - in real code, this would start with config
            Self::start().await
        }

        async fn stop(self) {
            self.handle.abort();
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }

    #[derive(Default)]
    struct DaemonConfig {
        max_sessions: usize,
    }

    use std::time::Duration;
}
```

### Manual Testing Checklist

Execute these tests manually to verify error handling in realistic scenarios:

1. **Kill daemon while TUI running**
   ```bash
   # Terminal 1: Start TUI
   atm

   # Terminal 2: Kill daemon
   pkill atmd

   # Expected: TUI shows "Disconnected" banner, attempts reconnection
   # Verify: TUI doesn't crash, shows reconnection attempts
   ```

2. **Fill registry to maximum**
   ```bash
   # Start daemon with low limit for testing
   MAX_SESSIONS=5 atmd start

   # Create more sessions than limit
   for i in {1..10}; do
     ./test-register-session.sh "session-$i"
   done

   # Expected: First 5 succeed, remaining return "registry full" error
   # Verify: Check daemon logs for rejection messages
   ```

3. **Send invalid JSON to daemon**
   ```bash
   # Connect to socket and send garbage
   echo "{ not valid json }" | nc -U /tmp/atm.sock

   # Expected: Daemon logs parse error but continues running
   # Verify: Other clients can still connect and function
   ```

4. **Network partition simulation**
   ```bash
   # Rename socket to simulate disconnect
   mv /tmp/atm.sock /tmp/atm.sock.bak

   # Run status line script
   echo '{"test": "data"}' | ./atm-status.sh

   # Expected: Script exits 0 immediately, no hang
   # Verify: Script completes in < 1 second
   ```

5. **Daemon restart with active TUI**
   ```bash
   # Terminal 1: Start TUI
   atm

   # Terminal 2: Restart daemon
   systemctl restart atmd
   # or: atmd stop && atmd start

   # Expected: TUI reconnects automatically
   # Verify: TUI shows brief disconnect then reconnects
   ```

---

## Summary

| Scenario | Behavior | User Experience |
|----------|----------|-----------------|
| Daemon unavailable | Bash scripts exit 0 silently | Claude Code unaffected |
| Malformed message | Log error, continue processing | No visible impact |
| Registry full | Reject new sessions, return error | Clear error message |
| TUI disconnected | Show banner, retry with backoff | See reconnection status |
| Protocol mismatch | Reject connection, clear message | Instructions to upgrade |
| Resource exhaustion | Reject new connections, serve existing | Existing clients continue |
