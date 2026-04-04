# Agent Tmux Manager Concurrency Model

> **⚠️ Panic-Free Policy:** All code in this document follows the panic-free guidelines from `CLAUDE.md`.
> No `.unwrap()`, `.expect()`, `panic!()`, or direct indexing in production code.
> Use `?`, `.ok()`, `.unwrap_or()`, `.unwrap_or_default()`, or pattern matching instead.

## Overview

ATM daemon uses an **Actor Model** for session registry to ensure data consistency without explicit locking. This document defines the concurrency architecture, message types, implementation patterns, and rationale for the chosen approach.

## Architecture

### Registry Actor

The session registry is managed by a single tokio task that owns all session data:

- **Single owner**: One tokio task owns the `SessionRegistry`
- **Message passing**: All mutations go through a channel-based command interface
- **Serial access**: Guarantees no race conditions (commands processed one at a time)
- **Event publishing**: Notifies subscribers of registry changes via broadcast channel

```
                                    ┌─────────────────────┐
                                    │   RegistryActor     │
                                    │                     │
   ┌──────────────┐                 │  ┌───────────────┐  │
   │ Status Line  │─────┐           │  │   registry    │  │
   │   Scripts    │     │           │  │  (HashMap)    │  │
   └──────────────┘     │           │  └───────────────┘  │
                        │           │                     │
   ┌──────────────┐     │  mpsc     │  ┌───────────────┐  │
   │    Hooks     │─────┼─────────► │  │infrastructure │  │
   │   Scripts    │     │  channel  │  │  (HashMap)    │  │
   └──────────────┘     │           │  └───────────────┘  │
                        │           │                     │
   ┌──────────────┐     │           │  ┌───────────────┐  │     broadcast
   │  TUI Client  │─────┘           │  │event_publisher│──┼─────────────►
   └──────────────┘                 │  │  (broadcast)  │  │     channel
                                    │  └───────────────┘  │
                                    └─────────────────────┘
```

## Message Types

### RegistryCommand Enum

All interactions with the registry actor are defined through the `RegistryCommand` enum. Each command includes a oneshot channel for response delivery.

```rust
use tokio::sync::{mpsc, oneshot, broadcast};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

/// Commands that can be sent to the RegistryActor
pub enum RegistryCommand {
    /// Register a new session with the registry
    Register {
        session: SessionDomain,
        respond_to: oneshot::Sender<Result<(), RegistryError>>,
    },

    /// Update context usage for an existing session
    UpdateContext {
        session_id: SessionId,
        context: ContextUsage,
        respond_to: oneshot::Sender<Result<(), RegistryError>>,
    },

    /// Update session status (e.g., active, idle, permission_requested)
    UpdateStatus {
        session_id: SessionId,
        status: SessionStatus,
        respond_to: oneshot::Sender<Result<(), RegistryError>>,
    },

    /// Retrieve a single session by ID (returns read-only view)
    GetSession {
        session_id: SessionId,
        respond_to: oneshot::Sender<Option<SessionView>>,
    },

    /// Retrieve all sessions (returns read-only views)
    GetAllSessions {
        respond_to: oneshot::Sender<Vec<SessionView>>,
    },

    /// Remove a session from the registry
    Unregister {
        session_id: SessionId,
        respond_to: oneshot::Sender<Result<(), RegistryError>>,
    },

    /// Record a tool usage event for a session
    RecordToolUse {
        session_id: SessionId,
        tool_name: String,
        respond_to: oneshot::Sender<Result<(), RegistryError>>,
    },

    /// Internal: Trigger cleanup of stale sessions
    CleanupStale {
        respond_to: oneshot::Sender<usize>, // Returns count of removed sessions
    },
}
```

### Supporting Types

```rust
use serde::{Serialize, Deserialize};
use thiserror::Error;

/// Unique identifier for a session
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Domain model for a session (owned by registry)
#[derive(Debug, Clone)]
pub struct SessionDomain {
    pub id: SessionId,
    pub agent_type: AgentType,
    pub status: SessionStatus,
    pub context: ContextUsage,
    pub started_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub pid: Option<u32>,
    pub working_directory: Option<String>,
}

/// Read-only view of a session (returned to clients)
#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    pub id: SessionId,
    pub agent_type: AgentType,
    pub status: SessionStatus,
    pub context: ContextUsage,
    pub started_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub is_stale: bool,
}

impl From<&SessionDomain> for SessionView {
    fn from(session: &SessionDomain) -> Self {
        let now = Utc::now();
        let since_activity = now.signed_duration_since(session.last_activity);

        SessionView {
            id: session.id.clone(),
            agent_type: session.agent_type.clone(),
            status: session.status.clone(),
            context: session.context.clone(),
            started_at: session.started_at,
            last_activity: session.last_activity,
            is_stale: since_activity > chrono::Duration::seconds(90),
        }
    }
}

/// Context window usage information
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextUsage {
    pub used_percentage: f64,
    pub total_cost_usd: f64,
    pub total_duration_ms: u64,
    pub lines_of_code: u64,
}

/// Type of AI agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentType {
    ClaudeCode,
    Custom(String),
}

/// Current status of a session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionStatus {
    Active,
    Idle,
    PermissionRequested { tool: String },
    Stale,
}

/// Errors that can occur in registry operations
#[derive(Error, Debug)]
pub enum RegistryError {
    #[error("Registry is full (max: {max} sessions)")]
    RegistryFull { max: usize },

    #[error("Session not found: {0}")]
    SessionNotFound(SessionId),

    #[error("Session already exists: {0}")]
    SessionAlreadyExists(SessionId),

    #[error("Channel send error")]
    ChannelSendError,

    #[error("Channel receive error")]
    ChannelReceiveError,
}

/// Events published by the registry
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Registered {
        session_id: SessionId,
        agent_type: AgentType,
        timestamp: DateTime<Utc>,
    },
    Updated {
        session_id: SessionId,
        session: SessionView,
        timestamp: DateTime<Utc>,
    },
    StatusChanged {
        session_id: SessionId,
        old_status: SessionStatus,
        new_status: SessionStatus,
        timestamp: DateTime<Utc>,
    },
    Removed {
        session_id: SessionId,
        reason: RemovalReason,
        timestamp: DateTime<Utc>,
    },
}

#[derive(Debug, Clone)]
pub enum RemovalReason {
    Unregistered,
    Stale,
    Expired,
}
```

## RegistryActor Implementation

### Core Actor Structure

```rust
use tokio::sync::{mpsc, oneshot, broadcast};
use std::collections::HashMap;

/// Resource limits
pub const MAX_SESSIONS: usize = 100;
pub const STALE_THRESHOLD_SECS: i64 = 90;
pub const MAX_SESSION_AGE_HOURS: i64 = 24;

/// The actor that owns and manages all session data
pub struct RegistryActor {
    /// Channel for receiving commands
    receiver: mpsc::Receiver<RegistryCommand>,

    /// Domain data: session business logic
    registry: HashMap<SessionId, SessionDomain>,

    /// Infrastructure data: per-session operational state
    infrastructure: HashMap<SessionId, SessionInfrastructure>,

    /// Publisher for session events (TUI clients subscribe)
    event_publisher: broadcast::Sender<SessionEvent>,
}

/// Infrastructure-level data for a session
#[derive(Debug)]
struct SessionInfrastructure {
    /// Recent tool usage for this session (bounded queue, FIFO)
    recent_tools: std::collections::VecDeque<ToolUsage>,

    /// Number of updates received
    update_count: u64,
}

#[derive(Debug, Clone)]
struct ToolUsage {
    tool_name: String,
    timestamp: DateTime<Utc>,
}

impl RegistryActor {
    /// Create a new RegistryActor with its command channel
    pub fn new(buffer_size: usize) -> (Self, RegistryHandle, broadcast::Receiver<SessionEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(buffer_size);
        let (event_tx, event_rx) = broadcast::channel(100);

        let actor = Self {
            receiver: cmd_rx,
            registry: HashMap::new(),
            infrastructure: HashMap::new(),
            event_publisher: event_tx,
        };

        let handle = RegistryHandle { sender: cmd_tx };

        (actor, handle, event_rx)
    }

    /// Run the actor's main loop (call this in a spawned task)
    pub async fn run(mut self) {
        tracing::info!("RegistryActor started");

        while let Some(cmd) = self.receiver.recv().await {
            self.handle_command(cmd);
        }

        tracing::info!("RegistryActor stopped (channel closed)");
    }

    /// Dispatch command to appropriate handler
    fn handle_command(&mut self, cmd: RegistryCommand) {
        match cmd {
            RegistryCommand::Register { session, respond_to } => {
                let result = self.handle_register(session);
                let _ = respond_to.send(result);
            }

            RegistryCommand::UpdateContext { session_id, context, respond_to } => {
                let result = self.handle_update_context(session_id, context);
                let _ = respond_to.send(result);
            }

            RegistryCommand::UpdateStatus { session_id, status, respond_to } => {
                let result = self.handle_update_status(session_id, status);
                let _ = respond_to.send(result);
            }

            RegistryCommand::GetSession { session_id, respond_to } => {
                let result = self.handle_get_session(session_id);
                let _ = respond_to.send(result);
            }

            RegistryCommand::GetAllSessions { respond_to } => {
                let result = self.handle_get_all_sessions();
                let _ = respond_to.send(result);
            }

            RegistryCommand::Unregister { session_id, respond_to } => {
                let result = self.handle_unregister(session_id);
                let _ = respond_to.send(result);
            }

            RegistryCommand::RecordToolUse { session_id, tool_name, respond_to } => {
                let result = self.handle_record_tool_use(session_id, tool_name);
                let _ = respond_to.send(result);
            }

            RegistryCommand::CleanupStale { respond_to } => {
                let count = self.handle_cleanup_stale();
                let _ = respond_to.send(count);
            }
        }
    }
}
```

### Command Handlers

```rust
impl RegistryActor {
    /// Register a new session
    fn handle_register(&mut self, session: SessionDomain) -> Result<(), RegistryError> {
        // Check capacity
        if self.registry.len() >= MAX_SESSIONS {
            tracing::warn!(
                session_id = %session.id,
                current = self.registry.len(),
                max = MAX_SESSIONS,
                "Registry full, rejecting registration"
            );
            return Err(RegistryError::RegistryFull { max: MAX_SESSIONS });
        }

        // Check for duplicate
        if self.registry.contains_key(&session.id) {
            return Err(RegistryError::SessionAlreadyExists(session.id));
        }

        let session_id = session.id.clone();
        let agent_type = session.agent_type.clone();

        // Insert session
        self.registry.insert(session_id.clone(), session);
        self.infrastructure.insert(session_id.clone(), SessionInfrastructure {
            recent_tools: std::collections::VecDeque::new(),
            update_count: 0,
        });

        // Publish event
        let _ = self.event_publisher.send(SessionEvent::Registered {
            session_id: session_id.clone(),
            agent_type,
            timestamp: Utc::now(),
        });

        tracing::info!(session_id = %session_id, "Session registered");

        Ok(())
    }

    /// Update context usage for a session
    fn handle_update_context(
        &mut self,
        session_id: SessionId,
        context: ContextUsage,
    ) -> Result<(), RegistryError> {
        let session = self.registry
            .get_mut(&session_id)
            .ok_or_else(|| RegistryError::SessionNotFound(session_id.clone()))?;

        session.context = context;
        session.last_activity = Utc::now();

        // Increment update count
        if let Some(infra) = self.infrastructure.get_mut(&session_id) {
            infra.update_count += 1;
        }

        // Publish update event
        let view = SessionView::from(&*session);
        let _ = self.event_publisher.send(SessionEvent::Updated {
            session_id,
            session: view,
            timestamp: Utc::now(),
        });

        Ok(())
    }

    /// Update session status
    fn handle_update_status(
        &mut self,
        session_id: SessionId,
        new_status: SessionStatus,
    ) -> Result<(), RegistryError> {
        let session = self.registry
            .get_mut(&session_id)
            .ok_or_else(|| RegistryError::SessionNotFound(session_id.clone()))?;

        let old_status = session.status.clone();
        session.status = new_status.clone();
        session.last_activity = Utc::now();

        // Publish status change event
        let _ = self.event_publisher.send(SessionEvent::StatusChanged {
            session_id,
            old_status,
            new_status,
            timestamp: Utc::now(),
        });

        Ok(())
    }

    /// Get a single session by ID
    fn handle_get_session(&self, session_id: SessionId) -> Option<SessionView> {
        self.registry
            .get(&session_id)
            .map(SessionView::from)
    }

    /// Get all sessions
    fn handle_get_all_sessions(&self) -> Vec<SessionView> {
        self.registry
            .values()
            .map(SessionView::from)
            .collect()
    }

    /// Unregister a session
    fn handle_unregister(&mut self, session_id: SessionId) -> Result<(), RegistryError> {
        if self.registry.remove(&session_id).is_none() {
            return Err(RegistryError::SessionNotFound(session_id));
        }

        self.infrastructure.remove(&session_id);

        let _ = self.event_publisher.send(SessionEvent::Removed {
            session_id: session_id.clone(),
            reason: RemovalReason::Unregistered,
            timestamp: Utc::now(),
        });

        tracing::info!(session_id = %session_id, "Session unregistered");

        Ok(())
    }

    /// Record tool usage for a session
    fn handle_record_tool_use(
        &mut self,
        session_id: SessionId,
        tool_name: String,
    ) -> Result<(), RegistryError> {
        // Verify session exists
        if !self.registry.contains_key(&session_id) {
            return Err(RegistryError::SessionNotFound(session_id));
        }

        // Record tool usage
        if let Some(infra) = self.infrastructure.get_mut(&session_id) {
            // Use VecDeque as a bounded FIFO queue
            // push_back + pop_front is O(1) and panic-free
            infra.recent_tools.push_back(ToolUsage {
                tool_name,
                timestamp: Utc::now(),
            });

            // Keep only last 100 tool usages
            while infra.recent_tools.len() > 100 {
                infra.recent_tools.pop_front();
            }
        }

        Ok(())
    }

    /// Clean up stale sessions (called periodically)
    fn handle_cleanup_stale(&mut self) -> usize {
        let now = Utc::now();
        let mut to_remove = Vec::new();

        for (id, session) in &self.registry {
            let since_activity = now.signed_duration_since(session.last_activity);
            let since_start = now.signed_duration_since(session.started_at);

            // Remove if stale (no activity for 90 seconds)
            if since_activity > chrono::Duration::seconds(STALE_THRESHOLD_SECS) {
                to_remove.push((id.clone(), RemovalReason::Stale));
            }
            // Remove if too old (even if active)
            else if since_start > chrono::Duration::hours(MAX_SESSION_AGE_HOURS) {
                to_remove.push((id.clone(), RemovalReason::Expired));
            }
        }

        let count = to_remove.len();

        for (id, reason) in to_remove {
            self.registry.remove(&id);
            self.infrastructure.remove(&id);

            tracing::info!(
                session_id = %id,
                reason = ?reason,
                "Cleaned up session"
            );

            let _ = self.event_publisher.send(SessionEvent::Removed {
                session_id: id,
                reason,
                timestamp: Utc::now(),
            });
        }

        count
    }
}
```

## RegistryHandle Client Interface

The `RegistryHandle` provides a clean async API for interacting with the registry actor. It is `Clone` and can be shared across multiple tasks.

```rust
/// Client interface for the RegistryActor
#[derive(Clone)]
pub struct RegistryHandle {
    sender: mpsc::Sender<RegistryCommand>,
}

impl RegistryHandle {
    /// Register a new session
    pub async fn register(&self, session: SessionDomain) -> Result<(), RegistryError> {
        let (tx, rx) = oneshot::channel();

        self.sender
            .send(RegistryCommand::Register {
                session,
                respond_to: tx,
            })
            .await
            .map_err(|_| RegistryError::ChannelSendError)?;

        rx.await.map_err(|_| RegistryError::ChannelReceiveError)?
    }

    /// Update context usage for a session
    pub async fn update_context(
        &self,
        session_id: SessionId,
        context: ContextUsage,
    ) -> Result<(), RegistryError> {
        let (tx, rx) = oneshot::channel();

        self.sender
            .send(RegistryCommand::UpdateContext {
                session_id,
                context,
                respond_to: tx,
            })
            .await
            .map_err(|_| RegistryError::ChannelSendError)?;

        rx.await.map_err(|_| RegistryError::ChannelReceiveError)?
    }

    /// Update session status
    pub async fn update_status(
        &self,
        session_id: SessionId,
        status: SessionStatus,
    ) -> Result<(), RegistryError> {
        let (tx, rx) = oneshot::channel();

        self.sender
            .send(RegistryCommand::UpdateStatus {
                session_id,
                status,
                respond_to: tx,
            })
            .await
            .map_err(|_| RegistryError::ChannelSendError)?;

        rx.await.map_err(|_| RegistryError::ChannelReceiveError)?
    }

    /// Get a session by ID
    ///
    /// Returns None if session not found OR if registry actor is unreachable.
    /// For distinguishing these cases, use a Result-returning variant.
    pub async fn get_session(&self, session_id: SessionId) -> Option<SessionView> {
        let (tx, rx) = oneshot::channel();

        if self.sender
            .send(RegistryCommand::GetSession {
                session_id: session_id.clone(),
                respond_to: tx,
            })
            .await
            .is_err()
        {
            tracing::warn!(session_id = %session_id, "get_session: actor unreachable");
            return None;
        }

        rx.await.unwrap_or_else(|_| {
            tracing::warn!(session_id = %session_id, "get_session: response channel closed");
            None
        })
    }

    /// Get all sessions
    ///
    /// Returns empty Vec if registry actor is unreachable.
    pub async fn get_all_sessions(&self) -> Vec<SessionView> {
        let (tx, rx) = oneshot::channel();

        if self.sender
            .send(RegistryCommand::GetAllSessions {
                respond_to: tx,
            })
            .await
            .is_err()
        {
            tracing::warn!("get_all_sessions: actor unreachable");
            return Vec::new();
        }

        rx.await.unwrap_or_else(|_| {
            tracing::warn!("get_all_sessions: response channel closed");
            Vec::new()
        })
    }

    /// Unregister a session
    pub async fn unregister(&self, session_id: SessionId) -> Result<(), RegistryError> {
        let (tx, rx) = oneshot::channel();

        self.sender
            .send(RegistryCommand::Unregister {
                session_id,
                respond_to: tx,
            })
            .await
            .map_err(|_| RegistryError::ChannelSendError)?;

        rx.await.map_err(|_| RegistryError::ChannelReceiveError)?
    }

    /// Record tool usage
    pub async fn record_tool_use(
        &self,
        session_id: SessionId,
        tool_name: String,
    ) -> Result<(), RegistryError> {
        let (tx, rx) = oneshot::channel();

        self.sender
            .send(RegistryCommand::RecordToolUse {
                session_id,
                tool_name,
                respond_to: tx,
            })
            .await
            .map_err(|_| RegistryError::ChannelSendError)?;

        rx.await.map_err(|_| RegistryError::ChannelReceiveError)?
    }

    /// Trigger cleanup of stale sessions
    ///
    /// Returns the number of sessions cleaned up, or an error if the actor
    /// is unreachable (channel closed).
    pub async fn cleanup_stale(&self) -> Result<usize, RegistryError> {
        let (tx, rx) = oneshot::channel();

        self.sender
            .send(RegistryCommand::CleanupStale {
                respond_to: tx,
            })
            .await
            .map_err(|_| RegistryError::ChannelSendError)?;

        rx.await.map_err(|_| RegistryError::ChannelReceiveError)
    }
}
```

## Why Actor Model?

### Advantages

| Benefit | Explanation |
|---------|-------------|
| **Zero lock contention** | No `Arc<Mutex>` or `Arc<RwLock>` needed. The actor owns data exclusively. |
| **Impossible deadlocks** | No locks to hold incorrectly or in wrong order. |
| **Easy to reason about** | Single-threaded access within the actor. State changes are sequential. |
| **Natural fit for Rust** | Ownership model aligns with Rust's borrowing rules. |
| **Built-in backpressure** | Bounded mpsc channel naturally limits message rate. |
| **Observable** | All mutations go through defined message types; easy to log/trace. |
| **Testable** | Can send commands directly and verify responses without mocking. |

### Disadvantages

| Drawback | Mitigation |
|----------|------------|
| **More boilerplate** | Command enums and oneshot channels add code. Use macros if needed. |
| **All ops are async** | Even reads require `await`. Acceptable for our use case. |
| **Single point of processing** | Actor processes commands sequentially. Sufficient for <1000 msg/sec. |

## Alternative: RwLock (Not Chosen)

We considered using `Arc<RwLock<HashMap>>` for the registry:

```rust
// Alternative approach (NOT CHOSEN)
pub struct SessionRegistry {
    sessions: Arc<RwLock<HashMap<SessionId, Session>>>,
}

impl SessionRegistry {
    pub async fn get_session(&self, id: &SessionId) -> Option<Session> {
        let guard = self.sessions.read().await;
        guard.get(id).cloned()
    }

    pub async fn register(&self, session: Session) -> Result<(), Error> {
        let mut guard = self.sessions.write().await;
        // ... mutation logic
    }
}
```

### Why RwLock Was Rejected

1. **Lock contention under load**: Multiple readers are fine, but writers block everyone. With frequent status updates (~3Hz per session), write contention becomes significant.

2. **Deadlock risk with async**: Holding a lock across `.await` points can cause deadlocks. Easy to accidentally write:
   ```rust
   // DANGEROUS: lock held across await
   let mut guard = registry.write().await;
   let session = guard.get(&id);
   some_async_operation(session).await;  // Still holding lock!
   ```

3. **No clear lock hierarchy**: When multiple resources need locking, ordering becomes critical. Actor model eliminates this concern.

4. **Harder to test**: Lock-based code requires careful setup to test contention scenarios.

## Idiomatic Async Patterns

This section covers idiomatic async patterns in Rust/Tokio that Agent Tmux Manager follows throughout its codebase.

### Structured Concurrency with `tokio::select!`

Use `select!` when you need to race multiple futures and respond to whichever completes first:

```rust
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tokio_util::sync::CancellationToken;

/// The main event loop pattern used by RegistryActor
async fn event_loop(
    mut cmd_rx: mpsc::Receiver<RegistryCommand>,
    cancel: CancellationToken,
) {
    let mut cleanup_interval = interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            // Bias toward commands (processed first if multiple ready)
            biased;

            // Handle incoming commands
            Some(cmd) = cmd_rx.recv() => {
                handle_command(cmd);
            }

            // Periodic cleanup
            _ = cleanup_interval.tick() => {
                cleanup_stale_sessions();
            }

            // Graceful shutdown
            _ = cancel.cancelled() => {
                tracing::info!("Shutdown requested, exiting event loop");
                break;
            }
        }
    }
}
```

**Key points:**
- Use `biased;` when ordering matters (commands before housekeeping)
- Always include a shutdown branch for graceful termination
- `select!` cancels unfinished branches when one completes

### Cancellation Safety

Not all futures are cancellation-safe. A future is cancellation-safe if dropping it partway through doesn't leave state inconsistent.

```rust
// CANCELLATION-SAFE: recv() is safe to cancel
tokio::select! {
    msg = receiver.recv() => { /* ... */ }
    _ = cancel.cancelled() => { break; }
}

// NOT CANCELLATION-SAFE: read_exact may have read partial data
tokio::select! {
    result = reader.read_exact(&mut buffer) => { /* ... */ }
    _ = cancel.cancelled() => { break; }  // buffer may be partially filled!
}

// FIX: Use cancellation-safe wrapper or don't select on non-safe futures
async fn read_message_safe(reader: &mut BufReader<OwnedReadHalf>) -> Option<String> {
    // read_line is cancellation-safe (returns complete lines or nothing)
    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => None,  // EOF
        Ok(_) => Some(line),
        Err(_) => None,
    }
}
```

**Cancellation-safe operations in Tokio:**
- `mpsc::Receiver::recv()`
- `oneshot::Receiver` (as a future)
- `broadcast::Receiver::recv()`
- `Notify::notified()`
- `BufReader::read_line()`

**NOT cancellation-safe:**
- `AsyncReadExt::read_exact()`
- `AsyncWriteExt::write_all()` (may have written partial data)
- Custom futures that hold partial state

### Timeout Patterns

Always use timeouts for external operations to prevent hangs:

```rust
use tokio::time::{timeout, Duration};

/// Send command with timeout
pub async fn send_with_timeout(
    handle: &RegistryHandle,
    session: SessionDomain,
    duration: Duration,
) -> Result<(), RegistryError> {
    timeout(duration, handle.register(session))
        .await
        .map_err(|_| RegistryError::Timeout)?
}

/// Connect with timeout and retry
pub async fn connect_with_timeout(
    socket_path: &Path,
    connect_timeout: Duration,
    max_retries: u32,
) -> Result<UnixStream, io::Error> {
    let mut attempts = 0;

    loop {
        match timeout(connect_timeout, UnixStream::connect(socket_path)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) if attempts < max_retries => {
                attempts += 1;
                tracing::warn!(attempt = attempts, error = %e, "Connection failed, retrying");
                tokio::time::sleep(Duration::from_millis(100 * attempts as u64)).await;
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                attempts += 1;
                if attempts >= max_retries {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "Connection timeout"));
                }
            }
        }
    }
}
```

### Graceful Shutdown with CancellationToken

Use `tokio_util::sync::CancellationToken` for cooperative shutdown:

```rust
use tokio_util::sync::CancellationToken;

pub struct Daemon {
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Daemon {
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            tasks: Vec::new(),
        }
    }

    pub fn spawn_task<F>(&mut self, name: &'static str, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let cancel = self.cancel.clone();
        self.tasks.push(tokio::spawn(async move {
            tokio::select! {
                _ = future => {
                    tracing::info!(task = name, "Task completed normally");
                }
                _ = cancel.cancelled() => {
                    tracing::info!(task = name, "Task cancelled");
                }
            }
        }));
    }

    pub async fn shutdown(self) {
        tracing::info!("Initiating graceful shutdown");

        // Signal all tasks to stop
        self.cancel.cancel();

        // Wait for all tasks with timeout
        let shutdown_timeout = Duration::from_secs(5);
        for (i, task) in self.tasks.into_iter().enumerate() {
            if timeout(shutdown_timeout, task).await.is_err() {
                tracing::warn!(task_index = i, "Task did not shutdown in time");
            }
        }

        tracing::info!("Shutdown complete");
    }
}
```

### `spawn` vs `spawn_blocking`

```rust
// Use spawn for async-native work
tokio::spawn(async move {
    let result = handle.get_all_sessions().await;
    broadcast_to_clients(result).await;
});

// Use spawn_blocking for CPU-intensive or blocking sync code
let compressed = tokio::task::spawn_blocking(move || {
    // This runs on a dedicated thread pool, won't block async runtime
    compress_large_data(&data)
}).await?;

// NEVER do this - blocks the async runtime!
async fn bad_example() {
    std::thread::sleep(Duration::from_secs(1));  // BAD!
    expensive_cpu_computation();  // BAD!
}

// DO this instead
async fn good_example() -> Result<(), Box<dyn std::error::Error>> {
    tokio::time::sleep(Duration::from_secs(1)).await;  // GOOD!
    tokio::task::spawn_blocking(|| expensive_cpu_computation())
        .await
        .map_err(|e| format!("Task panicked: {}", e))?;  // GOOD! Handle JoinError
    Ok(())
}
```

**When to use `spawn_blocking`:**
- File I/O without async wrappers
- CPU-intensive computations (compression, hashing)
- Calling sync libraries that might block
- Any operation taking >1ms synchronously

### Stream Processing

For processing sequences of async events:

```rust
use tokio_stream::{StreamExt, wrappers::ReceiverStream};

/// Process session events as a stream
async fn process_events(
    event_rx: broadcast::Receiver<SessionEvent>,
    client_tx: mpsc::Sender<DaemonMessage>,
) {
    let stream = tokio_stream::wrappers::BroadcastStream::new(event_rx);

    // Filter, map, and batch events
    let batched = stream
        .filter_map(|result| result.ok())  // Skip lagged errors
        .filter(|event| matches!(event, SessionEvent::Updated { .. }))
        .chunks_timeout(10, Duration::from_millis(100));  // Batch up to 10 or 100ms

    tokio::pin!(batched);

    while let Some(events) = batched.next().await {
        let message = DaemonMessage::BatchUpdate {
            sessions: events.into_iter()
                .filter_map(|e| match e {
                    SessionEvent::Updated { session, .. } => Some(session),
                    _ => None,
                })
                .collect(),
        };

        if client_tx.send(message).await.is_err() {
            break;  // Client disconnected
        }
    }
}
```

### Error Propagation in Async

Use the `?` operator with proper error types:

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum DaemonError {
    #[error("Registry error: {0}")]
    Registry(#[from] RegistryError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Channel closed")]
    ChannelClosed,

    #[error("Operation timed out")]
    Timeout,
}

/// Async function with proper error propagation
async fn handle_client(
    stream: UnixStream,
    registry: RegistryHandle,
) -> Result<(), DaemonError> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let msg: ClientMessage = serde_json::from_str(&line)
            .map_err(|e| DaemonError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;

        let response = match msg {
            ClientMessage::Register(session) => {
                registry.register(session).await?;  // ? works due to From impl
                DaemonMessage::Ok
            }
            ClientMessage::GetSessions => {
                let sessions = registry.get_all_sessions().await;
                DaemonMessage::Sessions(sessions)
            }
        };

        let json = serde_json::to_string(&response)?;
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }

    Ok(())
}
```

### Async Trait Methods

Use the `async-trait` crate for async methods in traits:

```rust
use async_trait::async_trait;

#[async_trait]
pub trait SessionStore {
    async fn get(&self, id: &SessionId) -> Option<SessionView>;
    async fn put(&self, session: SessionDomain) -> Result<(), RegistryError>;
    async fn remove(&self, id: &SessionId) -> Result<(), RegistryError>;
}

#[async_trait]
impl SessionStore for RegistryHandle {
    async fn get(&self, id: &SessionId) -> Option<SessionView> {
        self.get_session(id.clone()).await
    }

    async fn put(&self, session: SessionDomain) -> Result<(), RegistryError> {
        self.register(session).await
    }

    async fn remove(&self, id: &SessionId) -> Result<(), RegistryError> {
        self.unregister(id.clone()).await
    }
}
```

**Note:** `async-trait` uses heap allocation. For hot paths, consider returning `impl Future` or using manual polling.

## Async Boundaries

### Where `await` is Allowed

| Context | Example |
|---------|---------|
| Inside tokio tasks | `tokio::spawn(async { ... })` |
| In async functions | `async fn process() { ... }` |
| After acquiring oneshot responses | `let result = rx.await?;` |
| In select! branches | `tokio::select! { ... }` |

### Where `await` is NOT Allowed

| Context | Reason |
|---------|--------|
| Inside `Drop` implementations | `Drop::drop` is sync; cannot await |
| In sync callbacks | Callbacks like `Iterator::map` are sync |
| While holding `std::sync::Mutex` | Standard mutex is not async-aware |
| In `#[test]` without `#[tokio::test]` | Need async runtime |

### Safe Patterns

```rust
// GOOD: Await after oneshot response
let (tx, rx) = oneshot::channel();
sender.send(Command::Get { respond_to: tx }).await?;
let result = rx.await?;

// GOOD: Select between multiple futures
tokio::select! {
    cmd = receiver.recv() => handle_command(cmd),
    _ = shutdown.recv() => break,
}

// BAD: Holding lock across await
// NOTE: This uses std::sync::Mutex for illustration only.
// In real code, use tokio::sync::Mutex for async contexts.
// The lock().unwrap() shown below is for illustration - it would panic on poisoned mutex.
// DON'T DO THIS - holding std::sync::Mutex across await points can cause deadlocks:
//
//   let guard = mutex.lock().unwrap();
//   async_operation().await;  // BAD: lock held across await!
//   drop(guard);

// BETTER: Clone data, release lock, then await
// Still problematic because std::sync::Mutex::lock() can panic on poison.
// Prefer tokio::sync::Mutex in async contexts.
let data = {
    // Using ok() to avoid panic, falling back to default if lock is poisoned
    let guard = mutex.lock().ok();
    guard.map(|g| g.clone()).unwrap_or_default()
};
async_operation(data).await;  // OK: lock released

// BEST: Use tokio::sync::Mutex which is designed for async
// No panic possible - lock() returns a future, not a Result
let guard = async_mutex.lock().await;
let data = guard.clone();
drop(guard);
async_operation(data).await;
```

## Startup and Shutdown

### Starting the Actor

```rust
use tokio::sync::mpsc;

pub async fn start_daemon() -> Result<(), Box<dyn std::error::Error>> {
    // Create the actor and handle
    let (actor, registry_handle, event_rx) = RegistryActor::new(1000);

    // Spawn the actor task
    let actor_task = tokio::spawn(async move {
        actor.run().await;
    });

    // Spawn cleanup task
    let cleanup_handle = registry_handle.clone();
    let cleanup_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(30)
        );

        loop {
            interval.tick().await;
            match cleanup_handle.cleanup_stale().await {
                Ok(removed) if removed > 0 => {
                    tracing::info!(count = removed, "Cleaned up stale sessions");
                }
                Ok(_) => {} // No sessions cleaned up
                Err(e) => {
                    tracing::warn!(error = ?e, "Cleanup task failed to reach actor");
                }
            }
        }
    });

    // Use registry_handle for client connections...
    // event_rx for broadcasting to TUI clients...

    Ok(())
}
```

### Graceful Shutdown

```rust
use tokio::signal;

pub async fn run_with_shutdown(
    actor_task: tokio::task::JoinHandle<()>,
    cleanup_task: tokio::task::JoinHandle<()>,
) -> Result<(), std::io::Error> {
    // Wait for shutdown signal
    signal::ctrl_c().await?;

    tracing::info!("Shutdown signal received");

    // Abort cleanup task
    cleanup_task.abort();

    // Drop all RegistryHandles to close the channel
    // This will cause actor.run() to exit its loop

    // Wait for actor to finish
    let _ = actor_task.await;

    tracing::info!("Daemon shutdown complete");
}
```

## Testing Strategy

### Unit Tests for RegistryActor

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_session(id: &str) -> SessionDomain {
        SessionDomain {
            id: SessionId(id.to_string()),
            agent_type: AgentType::ClaudeCode,
            status: SessionStatus::Active,
            context: ContextUsage::default(),
            started_at: Utc::now(),
            last_activity: Utc::now(),
            pid: Some(12345),
            working_directory: Some("/home/user/project".to_string()),
        }
    }

    #[tokio::test]
    async fn test_register_and_get_session() {
        let (actor, handle, _events) = RegistryActor::new(100);
        tokio::spawn(actor.run());

        let session = create_test_session("test-1");

        // Register
        handle.register(session.clone()).await.unwrap();

        // Get
        let retrieved = handle.get_session(SessionId("test-1".to_string())).await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().id.0, "test-1");
    }

    #[tokio::test]
    async fn test_registry_full_error() {
        let (mut actor, handle, _events) = RegistryActor::new(100);

        // Override max for testing
        const TEST_MAX: usize = 2;

        tokio::spawn(async move {
            // Manually set max for this test
            while let Some(cmd) = actor.receiver.recv().await {
                if actor.registry.len() >= TEST_MAX {
                    if let RegistryCommand::Register { respond_to, .. } = cmd {
                        let _ = respond_to.send(Err(RegistryError::RegistryFull { max: TEST_MAX }));
                        continue;
                    }
                }
                actor.handle_command(cmd);
            }
        });

        // Fill registry
        handle.register(create_test_session("s1")).await.unwrap();
        handle.register(create_test_session("s2")).await.unwrap();

        // This should fail
        let result = handle.register(create_test_session("s3")).await;
        assert!(matches!(result, Err(RegistryError::RegistryFull { .. })));
    }

    #[tokio::test]
    async fn test_update_context() {
        let (actor, handle, _events) = RegistryActor::new(100);
        tokio::spawn(actor.run());

        let session = create_test_session("test-1");
        handle.register(session).await.unwrap();

        // Update context
        let new_context = ContextUsage {
            used_percentage: 50.0,
            total_cost_usd: 0.05,
            total_duration_ms: 5000,
            lines_of_code: 100,
        };

        handle.update_context(
            SessionId("test-1".to_string()),
            new_context.clone(),
        ).await.unwrap();

        // Verify
        let retrieved = handle.get_session(SessionId("test-1".to_string())).await.unwrap();
        assert_eq!(retrieved.context.used_percentage, 50.0);
    }

    #[tokio::test]
    async fn test_session_not_found() {
        let (actor, handle, _events) = RegistryActor::new(100);
        tokio::spawn(actor.run());

        let result = handle.update_context(
            SessionId("nonexistent".to_string()),
            ContextUsage::default(),
        ).await;

        assert!(matches!(result, Err(RegistryError::SessionNotFound(_))));
    }

    #[tokio::test]
    async fn test_event_publishing() {
        let (actor, handle, mut events) = RegistryActor::new(100);
        tokio::spawn(actor.run());

        let session = create_test_session("test-1");
        handle.register(session).await.unwrap();

        // Should receive registration event
        let event = events.recv().await.unwrap();
        assert!(matches!(event, SessionEvent::Registered { .. }));
    }
}
```

### Integration Tests with Multiple Concurrent Clients

```rust
#[tokio::test]
async fn test_concurrent_registrations() {
    let (actor, handle, _events) = RegistryActor::new(1000);
    tokio::spawn(actor.run());

    let mut tasks = Vec::new();

    // Spawn 50 concurrent registration tasks
    for i in 0..50 {
        let h = handle.clone();
        tasks.push(tokio::spawn(async move {
            let session = SessionDomain {
                id: SessionId(format!("session-{}", i)),
                agent_type: AgentType::ClaudeCode,
                status: SessionStatus::Active,
                context: ContextUsage::default(),
                started_at: Utc::now(),
                last_activity: Utc::now(),
                pid: None,
                working_directory: None,
            };
            h.register(session).await
        }));
    }

    // Wait for all registrations
    for task in tasks {
        task.await.unwrap().unwrap();
    }

    // Verify all sessions registered
    let all_sessions = handle.get_all_sessions().await;
    assert_eq!(all_sessions.len(), 50);
}

#[tokio::test]
async fn test_concurrent_reads_and_writes() {
    let (actor, handle, _events) = RegistryActor::new(1000);
    tokio::spawn(actor.run());

    // Pre-register some sessions
    for i in 0..10 {
        let session = SessionDomain {
            id: SessionId(format!("session-{}", i)),
            agent_type: AgentType::ClaudeCode,
            status: SessionStatus::Active,
            context: ContextUsage::default(),
            started_at: Utc::now(),
            last_activity: Utc::now(),
            pid: None,
            working_directory: None,
        };
        handle.register(session).await.unwrap();
    }

    let mut tasks = Vec::new();

    // Concurrent readers
    for _ in 0..20 {
        let h = handle.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..100 {
                let _ = h.get_all_sessions().await;
            }
        }));
    }

    // Concurrent writers
    for i in 0..10 {
        let h = handle.clone();
        tasks.push(tokio::spawn(async move {
            for j in 0..100 {
                let _ = h.update_context(
                    SessionId(format!("session-{}", i)),
                    ContextUsage {
                        used_percentage: j as f64,
                        ..Default::default()
                    },
                ).await;
            }
        }));
    }

    // All operations should complete without deadlock
    for task in tasks {
        task.await.unwrap();
    }
}
```

### Stress Tests

```rust
#[tokio::test]
#[ignore] // Run with: cargo test stress -- --ignored
async fn stress_test_high_message_rate() {
    let (actor, handle, _events) = RegistryActor::new(10000);
    tokio::spawn(actor.run());

    // Register sessions
    for i in 0..100 {
        let session = SessionDomain {
            id: SessionId(format!("session-{}", i)),
            agent_type: AgentType::ClaudeCode,
            status: SessionStatus::Active,
            context: ContextUsage::default(),
            started_at: Utc::now(),
            last_activity: Utc::now(),
            pid: None,
            working_directory: None,
        };
        handle.register(session).await.unwrap();
    }

    let start = std::time::Instant::now();
    let mut tasks = Vec::new();

    // Send 10,000 updates across 100 sessions
    for i in 0..100 {
        let h = handle.clone();
        tasks.push(tokio::spawn(async move {
            for j in 0..100 {
                let _ = h.update_context(
                    SessionId(format!("session-{}", i)),
                    ContextUsage {
                        used_percentage: j as f64,
                        total_cost_usd: 0.001 * j as f64,
                        total_duration_ms: j * 100,
                        lines_of_code: j * 10,
                    },
                ).await;
            }
        }));
    }

    for task in tasks {
        task.await.unwrap();
    }

    let elapsed = start.elapsed();
    let rate = 10_000.0 / elapsed.as_secs_f64();

    println!("Processed 10,000 updates in {:?} ({:.0} msg/sec)", elapsed, rate);

    // Should handle at least 1000 msg/sec
    assert!(rate > 1000.0, "Message rate too low: {}", rate);
}
```

## Summary

The Actor Model provides a robust, deadlock-free concurrency architecture for Agent Tmux Manager:

1. **RegistryActor** owns all session data and processes commands sequentially
2. **RegistryCommand** enum defines all possible operations with response channels
3. **RegistryHandle** provides an ergonomic async API for clients
4. **Event publishing** via broadcast channel notifies subscribers of changes
5. **Zero lock contention** ensures predictable performance under load

This design was chosen over RwLock-based alternatives for its simplicity, safety, and natural fit with Rust's ownership model.
