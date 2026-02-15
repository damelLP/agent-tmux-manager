//! Registry actor commands, errors, and events.
//!
//! This module defines the message types for communicating with the `RegistryActor`:
//! - `RegistryCommand`: Commands sent to the actor
//! - `RegistryError`: Errors that can occur during registry operations
//! - `SessionEvent`: Events published by the registry for subscribers
//!
//! All types are designed for async message passing and follow the panic-free policy.

use atm_core::{AgentType, HookEventType, SessionDomain, SessionId, SessionView};
use thiserror::Error;
use tokio::sync::oneshot;

// ============================================================================
// Registry Commands
// ============================================================================

/// Commands sent to the registry actor.
///
/// Each command uses a oneshot channel for the response, enabling
/// request-response patterns in async code without blocking.
///
/// # Usage
///
/// ```ignore
/// let (tx, rx) = oneshot::channel();
/// registry_tx.send(RegistryCommand::GetSession {
///     session_id: id,
///     respond_to: tx,
/// }).await?;
/// let session = rx.await?;
/// ```
#[derive(Debug)]
pub enum RegistryCommand {
    /// Register a new session in the registry.
    ///
    /// The session is boxed to reduce enum size variance.
    ///
    /// # Errors
    /// - `RegistryError::SessionAlreadyExists` if a session with this ID exists
    /// - `RegistryError::RegistryFull` if at maximum capacity
    Register {
        /// The session domain model to register (boxed for size optimization)
        session: Box<SessionDomain>,
        /// Channel to send the result
        respond_to: oneshot::Sender<Result<(), RegistryError>>,
    },

    /// Update a session from Claude Code status line data.
    ///
    /// Parses the raw JSON and applies updates to the session's
    /// cost, duration, context usage, and lines changed.
    ///
    /// # Errors
    /// - `RegistryError::SessionNotFound` if the session doesn't exist
    /// - `RegistryError::ParseError` if the JSON is malformed
    UpdateFromStatusLine {
        /// ID of the session to update
        session_id: SessionId,
        /// Raw status line JSON from Claude Code
        data: serde_json::Value,
        /// Channel to send the result
        respond_to: oneshot::Sender<Result<(), RegistryError>>,
    },

    /// Apply a hook event to a session.
    ///
    /// Updates the session's status based on the event type
    /// (e.g., PreToolUse sets RunningTool, PostToolUse sets Thinking).
    ///
    /// # Errors
    /// - `RegistryError::SessionNotFound` if the session doesn't exist
    ApplyHookEvent {
        /// ID of the session to update
        session_id: SessionId,
        /// Type of hook event
        event_type: HookEventType,
        /// Name of the tool (for tool-related events)
        tool_name: Option<String>,
        /// Notification type (for Notification events)
        notification_type: Option<String>,
        /// Process ID of the Claude Code process (for lifecycle tracking)
        pid: Option<u32>,
        /// Tmux pane ID if running in tmux
        tmux_pane: Option<String>,
        /// Channel to send the result
        respond_to: oneshot::Sender<Result<(), RegistryError>>,
    },

    /// Get a single session by ID.
    ///
    /// Returns `None` if the session doesn't exist.
    GetSession {
        /// ID of the session to retrieve
        session_id: SessionId,
        /// Channel to send the result
        respond_to: oneshot::Sender<Option<SessionView>>,
    },

    /// Get all sessions as views.
    ///
    /// Returns an empty vector if no sessions are registered.
    GetAllSessions {
        /// Channel to send the results
        respond_to: oneshot::Sender<Vec<SessionView>>,
    },

    /// Remove a session from the registry.
    ///
    /// # Errors
    /// - `RegistryError::SessionNotFound` if the session doesn't exist
    Remove {
        /// ID of the session to remove
        session_id: SessionId,
        /// Channel to send the result
        respond_to: oneshot::Sender<Result<(), RegistryError>>,
    },

    /// Trigger cleanup of dead-process sessions.
    ///
    /// This is a fire-and-forget command used by the cleanup task.
    /// Sessions whose Claude Code process has died are removed.
    CleanupStale,

    /// Register a discovered session (minimal data from /proc scan).
    ///
    /// Creates a minimal session with defaults that will be filled in
    /// when status line updates arrive.
    ///
    /// # Errors
    /// - `RegistryError::SessionAlreadyExists` if a session with this ID exists
    /// - `RegistryError::RegistryFull` if at maximum capacity
    RegisterDiscovered {
        /// ID of the discovered session (from transcript filename)
        session_id: SessionId,
        /// Process ID of the Claude Code process
        pid: u32,
        /// Working directory of the Claude process
        cwd: std::path::PathBuf,
        /// Tmux pane ID if running in tmux
        tmux_pane: Option<String>,
        /// Channel to send the result
        respond_to: oneshot::Sender<Result<(), RegistryError>>,
    },
}

// ============================================================================
// Registry Errors
// ============================================================================

/// Errors that can occur during registry operations.
///
/// Uses `thiserror` for ergonomic error handling and Display implementations.
#[derive(Debug, Clone, Error)]
pub enum RegistryError {
    /// The registry has reached its maximum session capacity.
    #[error("registry is full (max: {max} sessions)")]
    RegistryFull {
        /// Maximum number of sessions allowed
        max: usize,
    },

    /// The requested session was not found.
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),

    /// A session with this ID already exists.
    #[error("session already exists: {0}")]
    SessionAlreadyExists(SessionId),

    /// The response channel was closed before receiving a response.
    ///
    /// This typically indicates the actor was shut down.
    #[error("response channel closed")]
    ChannelClosed,

    /// Failed to parse status line or event data.
    #[error("parse error: {0}")]
    ParseError(String),
}

impl RegistryError {
    /// Creates a parse error from any error type.
    pub fn parse<E: std::fmt::Display>(err: E) -> Self {
        Self::ParseError(err.to_string())
    }
}

// ============================================================================
// Session Events
// ============================================================================

/// Events published by the registry to subscribers.
///
/// These events are broadcast to all connected TUI clients
/// via the broadcast channel.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// A new session was registered.
    Registered {
        /// ID of the registered session
        session_id: SessionId,
        /// Type of agent (main, explore, plan, etc.)
        agent_type: AgentType,
    },

    /// A session was updated (status, cost, context, etc.).
    ///
    /// The session view is boxed to reduce enum size variance.
    Updated {
        /// The updated session view (boxed for size optimization)
        session: Box<SessionView>,
    },

    /// A session was removed from the registry.
    Removed {
        /// ID of the removed session
        session_id: SessionId,
        /// Why the session was removed
        reason: RemovalReason,
    },
}

/// Reason why a session was removed from the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalReason {
    /// Client explicitly requested removal.
    Explicit,

    /// Removed to make room for new sessions when registry was full.
    RegistryFull,

    /// Claude Code sent SessionEnd hook event (session closed).
    SessionEnded,

    /// The Claude Code process died without sending SessionEnd hook.
    /// Detected via PID monitoring during cleanup.
    ProcessDied,

    /// Pending session was upgraded to a real session.
    /// The pending session (with temporary ID) is replaced by the real session.
    Upgraded,
}

impl std::fmt::Display for RemovalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Explicit => write!(f, "explicitly removed"),
            Self::RegistryFull => write!(f, "registry capacity reached"),
            Self::SessionEnded => write!(f, "session ended by Claude Code"),
            Self::ProcessDied => write!(f, "process died without SessionEnd"),
            Self::Upgraded => write!(f, "upgraded to real session"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::Model;

    #[test]
    fn test_registry_error_display() {
        let err = RegistryError::RegistryFull { max: 100 };
        assert_eq!(err.to_string(), "registry is full (max: 100 sessions)");

        let err = RegistryError::SessionNotFound(SessionId::new("test-123"));
        assert_eq!(err.to_string(), "session not found: test-123");

        let err = RegistryError::SessionAlreadyExists(SessionId::new("test-456"));
        assert_eq!(err.to_string(), "session already exists: test-456");

        let err = RegistryError::ChannelClosed;
        assert_eq!(err.to_string(), "response channel closed");

        let err = RegistryError::ParseError("invalid JSON".to_string());
        assert_eq!(err.to_string(), "parse error: invalid JSON");
    }

    #[test]
    fn test_registry_error_parse_helper() {
        let err = RegistryError::parse("something went wrong");
        assert!(matches!(err, RegistryError::ParseError(_)));
        assert_eq!(err.to_string(), "parse error: something went wrong");
    }

    #[test]
    fn test_removal_reason_display() {
        assert_eq!(RemovalReason::Explicit.to_string(), "explicitly removed");
        assert_eq!(
            RemovalReason::RegistryFull.to_string(),
            "registry capacity reached"
        );
        assert_eq!(
            RemovalReason::SessionEnded.to_string(),
            "session ended by Claude Code"
        );
        assert_eq!(
            RemovalReason::ProcessDied.to_string(),
            "process died without SessionEnd"
        );
    }

    #[test]
    fn test_session_event_variants() {
        // Test that all event types can be created and cloned
        let registered = SessionEvent::Registered {
            session_id: SessionId::new("test-1"),
            agent_type: AgentType::GeneralPurpose,
        };
        let _cloned = registered.clone();

        let session =
            SessionDomain::new(SessionId::new("test-2"), AgentType::Explore, Model::Sonnet4);
        let updated = SessionEvent::Updated {
            session: Box::new(SessionView::from_domain(&session)),
        };
        let _cloned = updated.clone();

        let removed = SessionEvent::Removed {
            session_id: SessionId::new("test-3"),
            reason: RemovalReason::ProcessDied,
        };
        let _cloned = removed.clone();
    }

    #[tokio::test]
    async fn test_command_oneshot_pattern() {
        // Verify the oneshot channel pattern works correctly
        let (tx, rx) = oneshot::channel::<Result<(), RegistryError>>();

        // Simulate actor receiving and responding
        tokio::spawn(async move {
            tx.send(Ok(())).ok();
        });

        // Verify we can receive the response
        let result = rx.await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn test_command_channel_closed_error() {
        // Verify behavior when channel is dropped
        let (tx, rx) = oneshot::channel::<Result<(), RegistryError>>();

        // Drop sender without sending
        drop(tx);

        // Receiver should get an error
        let result = rx.await;
        assert!(result.is_err());
    }
}
