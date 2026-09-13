//! Client interface for interacting with the RegistryActor.
//!
//! The `RegistryHandle` provides a cheap-to-clone interface for sending commands
//! to the registry actor and subscribing to session events.
//!
//! # Panic-Free Guarantees
//!
//! This module follows CLAUDE.md panic-free policy:
//! - No `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, `todo!()`
//! - All fallible operations use `?`, pattern matching, or `unwrap_or`
//! - Channel errors are mapped to `RegistryError::ChannelClosed`

use tokio::sync::{broadcast, mpsc, oneshot};

use atm_core::{Harness, LifecycleEvent, SessionDomain, SessionId, SessionView};

use super::commands::{RegistryCommand, RegistryError, SessionEvent};

// ============================================================================
// Registry Handle
// ============================================================================

/// Handle for interacting with the registry actor.
///
/// This is a cheap-to-clone handle that can be shared across tasks.
/// All methods are async and communicate with the actor via channels.
///
/// # Usage
///
/// ```ignore
/// // Clone the handle to share across tasks
/// let handle = registry_handle.clone();
///
/// // Register a session
/// handle.register(session).await?;
///
/// // Get all sessions
/// let sessions = handle.get_all_sessions().await;
///
/// // Subscribe to events
/// let mut rx = handle.subscribe();
/// while let Ok(event) = rx.recv().await {
///     // Handle event
/// }
/// ```
#[derive(Clone)]
pub struct RegistryHandle {
    /// Command sender to the actor
    sender: mpsc::Sender<RegistryCommand>,

    /// Event broadcaster for subscribing to updates
    event_sender: broadcast::Sender<SessionEvent>,
}

impl RegistryHandle {
    /// Create a new registry handle.
    ///
    /// # Arguments
    ///
    /// * `sender` - The command channel sender for communicating with the actor
    /// * `event_sender` - The broadcast sender for subscribing to events
    pub fn new(
        sender: mpsc::Sender<RegistryCommand>,
        event_sender: broadcast::Sender<SessionEvent>,
    ) -> Self {
        Self {
            sender,
            event_sender,
        }
    }

    /// Register a new session in the registry.
    ///
    /// # Errors
    ///
    /// - `RegistryError::SessionAlreadyExists` if a session with this ID exists
    /// - `RegistryError::RegistryFull` if the registry is at maximum capacity
    /// - `RegistryError::ChannelClosed` if the actor has shut down
    pub async fn register(&self, session: SessionDomain) -> Result<(), RegistryError> {
        let (tx, rx) = oneshot::channel();

        self.sender
            .send(RegistryCommand::Register {
                session: Box::new(session),
                respond_to: tx,
            })
            .await
            .map_err(|_| RegistryError::ChannelClosed)?;

        rx.await.map_err(|_| RegistryError::ChannelClosed)?
    }

    /// Update a session from Claude Code status line data.
    ///
    /// Parses the raw JSON and applies updates to the session's
    /// cost, duration, context usage, and lines changed.
    ///
    /// # Errors
    ///
    /// - `RegistryError::SessionNotFound` if the session doesn't exist
    /// - `RegistryError::ParseError` if the JSON is malformed
    /// - `RegistryError::ChannelClosed` if the actor has shut down
    pub async fn update_from_status_line(
        &self,
        session_id: SessionId,
        data: serde_json::Value,
    ) -> Result<(), RegistryError> {
        let (tx, rx) = oneshot::channel();

        self.sender
            .send(RegistryCommand::UpdateFromStatusLine {
                session_id,
                data,
                respond_to: tx,
            })
            .await
            .map_err(|_| RegistryError::ChannelClosed)?;

        rx.await.map_err(|_| RegistryError::ChannelClosed)?
    }

    /// Apply a vendor-neutral lifecycle event to a session.
    ///
    /// The connection layer translates raw vendor events (Claude
    /// `RawHookEvent`, future pi extension messages) into
    /// `LifecycleEvent` before calling this method.
    ///
    /// # Errors
    ///
    /// - `RegistryError::SessionNotFound` if the session doesn't exist
    /// - `RegistryError::ChannelClosed` if the actor has shut down
    pub async fn apply_lifecycle_event(
        &self,
        session_id: SessionId,
        event: LifecycleEvent,
        harness: Harness,
        pid: Option<u32>,
        tmux_pane: Option<String>,
    ) -> Result<(), RegistryError> {
        self.apply_lifecycle_event_from(session_id, event, harness, pid, tmux_pane, None)
            .await
    }

    /// Like [`Self::apply_lifecycle_event`], but tags the event with the
    /// in-process child agent it came from (Claude subagents and
    /// teammates share the parent's process and `session_id`), so the
    /// registry routes it to the child's own session.
    pub async fn apply_lifecycle_event_from(
        &self,
        session_id: SessionId,
        event: LifecycleEvent,
        harness: Harness,
        pid: Option<u32>,
        tmux_pane: Option<String>,
        child_agent: Option<atm_core::ChildAgentRef>,
    ) -> Result<(), RegistryError> {
        let (tx, rx) = oneshot::channel();

        self.sender
            .send(RegistryCommand::ApplyLifecycleEvent {
                session_id,
                event,
                harness,
                pid,
                tmux_pane,
                child_agent,
                respond_to: tx,
            })
            .await
            .map_err(|_| RegistryError::ChannelClosed)?;

        rx.await.map_err(|_| RegistryError::ChannelClosed)?
    }

    /// Records that child agent `agent_id` answers to `name` (the Agent
    /// tool `name` argument), so teammate events that only carry a name
    /// can be routed to its session. Fire-and-forget.
    ///
    /// # Errors
    ///
    /// - `RegistryError::ChannelClosed` if the actor has shut down
    pub async fn register_child_alias(
        &self,
        parent: SessionId,
        name: String,
        agent_id: String,
    ) -> Result<(), RegistryError> {
        self.sender
            .send(RegistryCommand::RegisterChildAlias {
                parent,
                name,
                agent_id,
            })
            .await
            .map_err(|_| RegistryError::ChannelClosed)
    }

    /// Get a single session by ID.
    ///
    /// Returns `None` if the session doesn't exist or if communication
    /// with the actor fails.
    pub async fn get_session(&self, session_id: SessionId) -> Option<SessionView> {
        let (tx, rx) = oneshot::channel();

        self.sender
            .send(RegistryCommand::GetSession {
                session_id,
                respond_to: tx,
            })
            .await
            .ok()?;

        rx.await.ok()?
    }

    /// Get all sessions as views.
    ///
    /// Returns an empty vector if no sessions are registered or if
    /// communication with the actor fails.
    pub async fn get_all_sessions(&self) -> Vec<SessionView> {
        let (tx, rx) = oneshot::channel();

        if self
            .sender
            .send(RegistryCommand::GetAllSessions { respond_to: tx })
            .await
            .is_err()
        {
            return Vec::new();
        }

        rx.await.unwrap_or_default()
    }

    /// Remove a session from the registry.
    ///
    /// # Errors
    ///
    /// - `RegistryError::SessionNotFound` if the session doesn't exist
    /// - `RegistryError::ChannelClosed` if the actor has shut down
    pub async fn remove(&self, session_id: SessionId) -> Result<(), RegistryError> {
        let (tx, rx) = oneshot::channel();

        self.sender
            .send(RegistryCommand::Remove {
                session_id,
                respond_to: tx,
            })
            .await
            .map_err(|_| RegistryError::ChannelClosed)?;

        rx.await.map_err(|_| RegistryError::ChannelClosed)?
    }

    /// Trigger cleanup of stale sessions.
    ///
    /// This is a fire-and-forget operation - it does not wait for
    /// the cleanup to complete or return any result.
    pub async fn cleanup_stale(&self) {
        // Fire-and-forget: ignore send errors (actor may be shutting down)
        let _ = self.sender.send(RegistryCommand::CleanupStale).await;
    }

    /// Register a discovered session (minimal data from /proc scan).
    ///
    /// Creates a minimal session with defaults that will be filled in
    /// when status line updates arrive. If the session already exists,
    /// this is a no-op (returns Ok).
    ///
    /// # Errors
    ///
    /// - `RegistryError::RegistryFull` if the registry is at maximum capacity
    /// - `RegistryError::ChannelClosed` if the actor has shut down
    pub async fn register_discovered(
        &self,
        session_id: SessionId,
        pid: u32,
        cwd: std::path::PathBuf,
        tmux_pane: Option<String>,
        harness: Harness,
    ) -> Result<(), RegistryError> {
        let (tx, rx) = oneshot::channel();

        self.sender
            .send(RegistryCommand::RegisterDiscovered {
                session_id,
                pid,
                cwd,
                tmux_pane,
                harness,
                respond_to: tx,
            })
            .await
            .map_err(|_| RegistryError::ChannelClosed)?;

        rx.await.map_err(|_| RegistryError::ChannelClosed)?
    }

    /// Subscribe to session events.
    ///
    /// Returns a broadcast receiver that will receive all session events
    /// (registrations, updates, removals) published by the registry actor.
    ///
    /// This is a synchronous operation - it doesn't communicate with the actor.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.event_sender.subscribe()
    }

    /// Check if the actor is still running.
    ///
    /// Returns `true` if the command channel is still open.
    pub fn is_connected(&self) -> bool {
        !self.sender.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::{AgentType, Model, Tool};

    fn create_test_handle() -> (RegistryHandle, mpsc::Receiver<RegistryCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let (event_tx, _event_rx) = broadcast::channel(16);
        let handle = RegistryHandle::new(cmd_tx, event_tx);
        (handle, cmd_rx)
    }

    fn create_test_session(id: &str) -> SessionDomain {
        SessionDomain::new(
            SessionId::new(id),
            AgentType::GeneralPurpose,
            Model::Sonnet4,
        )
    }

    #[tokio::test]
    async fn test_handle_is_clone() {
        let (handle, _rx) = create_test_handle();
        let _cloned = handle.clone();
        // Compiles = test passes
    }

    #[tokio::test]
    async fn test_register_sends_command() {
        let (handle, mut rx) = create_test_handle();

        let session = create_test_session("test-123");

        // Spawn task to handle the command
        let cmd_handler = tokio::spawn(async move {
            if let Some(RegistryCommand::Register {
                session,
                respond_to,
            }) = rx.recv().await
            {
                assert_eq!(session.id.as_str(), "test-123");
                let _ = respond_to.send(Ok(()));
                return true;
            }
            false
        });

        let result = handle.register(session).await;
        assert!(result.is_ok());
        assert!(cmd_handler.await.unwrap());
    }

    #[tokio::test]
    async fn test_register_channel_closed_error() {
        let (handle, rx) = create_test_handle();
        drop(rx); // Close the channel

        let session = create_test_session("test-123");
        let result = handle.register(session).await;

        assert!(matches!(result, Err(RegistryError::ChannelClosed)));
    }

    #[tokio::test]
    async fn test_get_session_returns_none_on_channel_close() {
        let (handle, rx) = create_test_handle();
        drop(rx);

        let result = handle.get_session(SessionId::new("test-123")).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_all_sessions_returns_empty_on_channel_close() {
        let (handle, rx) = create_test_handle();
        drop(rx);

        let result = handle.get_all_sessions().await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_cleanup_stale_fire_and_forget() {
        let (handle, mut rx) = create_test_handle();

        // Spawn task to receive the command
        let cmd_handler = tokio::spawn(async move {
            if let Some(RegistryCommand::CleanupStale) = rx.recv().await {
                return true;
            }
            false
        });

        handle.cleanup_stale().await;
        assert!(cmd_handler.await.unwrap());
    }

    #[tokio::test]
    async fn test_cleanup_stale_ignores_closed_channel() {
        let (handle, rx) = create_test_handle();
        drop(rx);

        // Should not panic or error
        handle.cleanup_stale().await;
    }

    #[tokio::test]
    async fn test_subscribe_returns_receiver() {
        let (handle, _rx) = create_test_handle();

        let _subscriber = handle.subscribe();
        // Compiles and returns = test passes
    }

    #[tokio::test]
    async fn test_is_connected() {
        let (handle, rx) = create_test_handle();

        assert!(handle.is_connected());

        drop(rx);
        // Need to send to detect closure
        let _ = handle.sender.send(RegistryCommand::CleanupStale).await;

        // After dropping receiver and attempting send, channel should be closed
        assert!(!handle.is_connected());
    }

    #[tokio::test]
    async fn test_update_from_status_line() {
        let (handle, mut rx) = create_test_handle();

        let cmd_handler = tokio::spawn(async move {
            if let Some(RegistryCommand::UpdateFromStatusLine {
                session_id,
                data,
                respond_to,
            }) = rx.recv().await
            {
                assert_eq!(session_id.as_str(), "test-123");
                assert!(data.get("model").is_some());
                let _ = respond_to.send(Ok(()));
                return true;
            }
            false
        });

        let data = serde_json::json!({
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.25}
        });

        let result = handle
            .update_from_status_line(SessionId::new("test-123"), data)
            .await;
        assert!(result.is_ok());
        assert!(cmd_handler.await.unwrap());
    }

    #[tokio::test]
    async fn test_apply_lifecycle_event() {
        let (handle, mut rx) = create_test_handle();

        let cmd_handler = tokio::spawn(async move {
            if let Some(RegistryCommand::ApplyLifecycleEvent {
                session_id,
                event,
                harness,
                pid,
                tmux_pane,
                child_agent: _,
                respond_to,
            }) = rx.recv().await
            {
                assert_eq!(session_id.as_str(), "test-123");
                assert_eq!(
                    event,
                    LifecycleEvent::ToolCallStart {
                        name: Tool::Bash,
                        tool_use_id: None,
                        input: None,
                    }
                );
                assert_eq!(harness, Harness::Pi);
                assert_eq!(pid, Some(12345));
                assert_eq!(tmux_pane, Some("%5".to_string()));
                let _ = respond_to.send(Ok(()));
                return true;
            }
            false
        });

        let result = handle
            .apply_lifecycle_event(
                SessionId::new("test-123"),
                LifecycleEvent::ToolCallStart {
                    name: Tool::Bash,
                    tool_use_id: None,
                    input: None,
                },
                Harness::Pi,
                Some(12345),
                Some("%5".to_string()),
            )
            .await;
        assert!(result.is_ok());
        assert!(cmd_handler.await.unwrap());
    }

    #[tokio::test]
    async fn test_remove() {
        let (handle, mut rx) = create_test_handle();

        let cmd_handler = tokio::spawn(async move {
            if let Some(RegistryCommand::Remove {
                session_id,
                respond_to,
            }) = rx.recv().await
            {
                assert_eq!(session_id.as_str(), "test-123");
                let _ = respond_to.send(Ok(()));
                return true;
            }
            false
        });

        let result = handle.remove(SessionId::new("test-123")).await;
        assert!(result.is_ok());
        assert!(cmd_handler.await.unwrap());
    }
}
