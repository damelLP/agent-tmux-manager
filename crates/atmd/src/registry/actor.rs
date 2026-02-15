//! Registry actor - owns all session state and processes commands.
//!
//! The RegistryActor is the single owner of session state in the system.
//! It receives commands via an mpsc channel and publishes events via broadcast.
//!
//! # Panic-Free Guarantees
//!
//! This module follows CLAUDE.md panic-free policy:
//! - No `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, `todo!()`
//! - All fallible operations use `?`, pattern matching, or `unwrap_or`
//! - Channel send failures are logged but don't panic

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::Utc;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use atm_core::{HookEventType, SessionDomain, SessionId, SessionInfrastructure, SessionView};
use atm_protocol::RawStatusLine;

use super::commands::{RegistryCommand, RegistryError, RemovalReason, SessionEvent};

// ============================================================================
// Resource Limits (from RESOURCE_LIMITS.md)
// ============================================================================

/// Maximum number of sessions the registry can hold.
pub const MAX_SESSIONS: usize = 100;

// ============================================================================
// Registry Actor
// ============================================================================

/// The registry actor - owns all session state.
///
/// Implements the actor pattern: receives commands via mpsc channel,
/// processes them sequentially, and publishes events to subscribers.
///
/// # Ownership
///
/// The actor owns:
/// - `sessions_by_pid`: HashMap of session data keyed by PID (primary key)
/// - `session_id_to_pid`: Index for session_id → PID lookups
///
/// # Design: PID as Primary Key
///
/// Using PID as the primary key eliminates session duplication issues that
/// occurred when discovery and status lines created separate entries for
/// the same Claude process. One PID = one session entry.
///
/// # Thread Safety
///
/// The actor runs in a single task and processes commands sequentially.
/// All state mutations happen within this single task.
pub struct RegistryActor {
    /// Command receiver
    receiver: mpsc::Receiver<RegistryCommand>,

    /// Primary session storage: PID → (SessionDomain, SessionInfrastructure)
    /// PID is the primary key because one Claude process = one session.
    sessions_by_pid: HashMap<u32, (SessionDomain, SessionInfrastructure)>,

    /// Index for session_id → PID lookups.
    /// Enables O(1) lookup when commands specify session_id.
    session_id_to_pid: HashMap<SessionId, u32>,

    /// Event publisher for real-time updates to TUI clients
    event_publisher: broadcast::Sender<SessionEvent>,
}

impl RegistryActor {
    /// Creates a new registry actor.
    ///
    /// # Arguments
    ///
    /// * `receiver` - Channel for receiving commands
    /// * `event_publisher` - Broadcast channel for publishing events
    pub fn new(
        receiver: mpsc::Receiver<RegistryCommand>,
        event_publisher: broadcast::Sender<SessionEvent>,
    ) -> Self {
        Self {
            receiver,
            sessions_by_pid: HashMap::new(),
            session_id_to_pid: HashMap::new(),
            event_publisher,
        }
    }

    /// Runs the actor event loop.
    ///
    /// Processes commands until the channel closes (all senders dropped).
    /// This is the main entry point - call this in a spawned task.
    pub async fn run(mut self) {
        info!("Registry actor starting");

        while let Some(cmd) = self.receiver.recv().await {
            self.handle_command(cmd);
        }

        info!(
            "Registry actor stopped (sessions: {})",
            self.sessions_by_pid.len()
        );
    }

    /// Dispatches a command to the appropriate handler.
    fn handle_command(&mut self, cmd: RegistryCommand) {
        match cmd {
            RegistryCommand::Register {
                session,
                respond_to,
            } => {
                // Register command doesn't include PID - used mainly for testing
                let result = self.handle_register(*session, None);
                // Ignore send error - client may have dropped the receiver
                let _ = respond_to.send(result);
            }
            RegistryCommand::UpdateFromStatusLine {
                session_id,
                data,
                respond_to,
            } => {
                let result = self.handle_update_from_status_line(session_id, data);
                let _ = respond_to.send(result);
            }
            RegistryCommand::ApplyHookEvent {
                session_id,
                event_type,
                tool_name,
                notification_type,
                pid,
                tmux_pane,
                respond_to,
            } => {
                let result = self.handle_apply_hook_event(
                    session_id,
                    event_type,
                    tool_name,
                    notification_type,
                    pid,
                    tmux_pane,
                );
                let _ = respond_to.send(result);
            }
            RegistryCommand::GetSession {
                session_id,
                respond_to,
            } => {
                let result = self.handle_get_session(&session_id);
                let _ = respond_to.send(result);
            }
            RegistryCommand::GetAllSessions { respond_to } => {
                let result = self.handle_get_all_sessions();
                let _ = respond_to.send(result);
            }
            RegistryCommand::Remove {
                session_id,
                respond_to,
            } => {
                let result = self.handle_remove(session_id, RemovalReason::Explicit);
                let _ = respond_to.send(result);
            }
            RegistryCommand::CleanupStale => {
                self.handle_cleanup_stale();
            }
            RegistryCommand::RegisterDiscovered {
                session_id,
                pid,
                cwd,
                tmux_pane,
                respond_to,
            } => {
                let result = self.handle_register_discovered(session_id, pid, cwd, tmux_pane);
                let _ = respond_to.send(result);
            }
        }
    }

    // ========================================================================
    // Command Handlers
    // ========================================================================

    /// Handles session registration.
    ///
    /// Note: This is now primarily used for testing. Most sessions are
    /// registered via `handle_register_discovered` or status line updates.
    /// Without a PID, this creates a session that cannot be looked up by PID.
    fn handle_register(
        &mut self,
        session: SessionDomain,
        pid: Option<u32>,
    ) -> Result<(), RegistryError> {
        // Check capacity
        if self.sessions_by_pid.len() >= MAX_SESSIONS {
            warn!(
                session_id = %session.id,
                current = self.sessions_by_pid.len(),
                max = MAX_SESSIONS,
                "Registry is full, rejecting registration"
            );
            return Err(RegistryError::RegistryFull { max: MAX_SESSIONS });
        }

        // Get or generate PID - we need a PID for the primary key
        let pid = match pid {
            Some(p) if p != 0 => p,
            _ => {
                // No valid PID provided - this is unusual but we handle it gracefully
                // by checking for duplicate session_id instead
                if self.session_id_to_pid.contains_key(&session.id) {
                    debug!(
                        session_id = %session.id,
                        "Session already exists (by session_id), rejecting registration"
                    );
                    return Err(RegistryError::SessionAlreadyExists(session.id));
                }
                // Generate a synthetic PID for storage (won't match any real process)
                // This is only for testing scenarios
                self.generate_synthetic_pid()
            }
        };

        // Check for duplicate by PID
        if self.sessions_by_pid.contains_key(&pid) {
            debug!(
                session_id = %session.id,
                pid = pid,
                "Session already exists for PID, rejecting registration"
            );
            return Err(RegistryError::SessionAlreadyExists(session.id));
        }

        let session_id = session.id.clone();
        let agent_type = session.agent_type.clone();

        // Create infrastructure and set PID
        let mut infra = SessionInfrastructure::new();
        infra.set_pid(pid);

        // Insert into primary storage and index
        self.sessions_by_pid.insert(pid, (session, infra));
        self.session_id_to_pid.insert(session_id.clone(), pid);

        info!(
            session_id = %session_id,
            pid = pid,
            agent_type = ?agent_type,
            total_sessions = self.sessions_by_pid.len(),
            "Session registered"
        );

        // Publish event (ignore if no subscribers)
        let _ = self.event_publisher.send(SessionEvent::Registered {
            session_id,
            agent_type,
        });

        Ok(())
    }

    /// Generates a synthetic PID for sessions without a real PID (testing only).
    fn generate_synthetic_pid(&self) -> u32 {
        // Use high PID range unlikely to conflict with real processes
        let base: u32 = 0x8000_0000;
        // Find the first unused synthetic PID
        for i in 0..u32::MAX {
            let candidate = base.wrapping_add(i);
            if !self.sessions_by_pid.contains_key(&candidate) {
                return candidate;
            }
        }
        // Should never happen - would need 2 billion sessions
        base
    }

    /// Handles registration of a discovered session.
    ///
    /// Creates a minimal session with defaults. The session will be updated
    /// with full data when status line updates arrive.
    ///
    /// With PID as primary key, if a session already exists for this PID,
    /// we update its session_id rather than creating a duplicate.
    fn handle_register_discovered(
        &mut self,
        session_id: SessionId,
        pid: u32,
        cwd: PathBuf,
        tmux_pane: Option<String>,
    ) -> Result<(), RegistryError> {
        // PID 0 is invalid
        if pid == 0 {
            warn!(
                session_id = %session_id,
                "Cannot register discovered session with PID 0"
            );
            return Ok(());
        }

        // Check if session already exists for this PID
        if let Some((existing_session, _)) = self.sessions_by_pid.get(&pid) {
            // If session_id is different, this is an update (e.g., pending → real)
            if existing_session.id != session_id {
                debug!(
                    old_id = %existing_session.id,
                    new_id = %session_id,
                    pid = pid,
                    "Updating session_id for existing PID"
                );
                // We'll handle this via upgrade logic below
            } else {
                // Same session_id, same PID - nothing to do
                debug!(
                    session_id = %session_id,
                    pid = pid,
                    "Discovered session already exists, skipping"
                );
                return Ok(());
            }
        }

        // Check capacity
        if !self.sessions_by_pid.contains_key(&pid) && self.sessions_by_pid.len() >= MAX_SESSIONS {
            warn!(
                session_id = %session_id,
                current = self.sessions_by_pid.len(),
                max = MAX_SESSIONS,
                "Registry is full, cannot register discovered session"
            );
            return Err(RegistryError::RegistryFull { max: MAX_SESSIONS });
        }

        // Create minimal session with defaults
        use atm_core::{AgentType, Model};
        let mut session = SessionDomain::new(
            session_id.clone(),
            AgentType::GeneralPurpose, // Will be updated when status line arrives
            Model::Unknown,            // Will be updated when status line arrives
        );
        // Set working directory from discovery
        session.working_directory = Some(cwd.to_string_lossy().to_string());
        // Set tmux pane from discovery
        session.tmux_pane = tmux_pane;
        let agent_type = session.agent_type.clone();

        // If there's an existing session for this PID, transfer its data
        let infra = if let Some((old_session, old_infra)) = self.sessions_by_pid.remove(&pid) {
            // Remove old session_id from index
            self.session_id_to_pid.remove(&old_session.id);

            // Publish removal event for old session
            let _ = self.event_publisher.send(SessionEvent::Removed {
                session_id: old_session.id.clone(),
                reason: RemovalReason::Upgraded,
            });

            // Preserve infrastructure data (tool history, update counts, etc.)
            old_infra
        } else {
            // Create new infrastructure with PID
            let mut infra = SessionInfrastructure::new();
            infra.set_pid(pid);
            infra
        };

        // Insert into primary storage and index
        self.sessions_by_pid.insert(pid, (session, infra));
        self.session_id_to_pid.insert(session_id.clone(), pid);

        info!(
            session_id = %session_id,
            pid = pid,
            total_sessions = self.sessions_by_pid.len(),
            "Discovered session registered"
        );

        // Publish event (ignore if no subscribers)
        let _ = self.event_publisher.send(SessionEvent::Registered {
            session_id: session_id.clone(),
            agent_type,
        });

        // Also publish an initial Updated event so TUI shows it
        if let Some((session, _)) = self.sessions_by_pid.get(&pid) {
            let view = SessionView::from_domain(session);
            let _ = self.event_publisher.send(SessionEvent::Updated {
                session: Box::new(view),
            });
        }

        Ok(())
    }

    /// Handles status line update.
    ///
    /// With PID as primary key, the logic is simplified:
    /// - If we have a PID, look up by PID and update (or create) the session
    /// - If no PID, fall back to session_id lookup
    fn handle_update_from_status_line(
        &mut self,
        session_id: SessionId,
        data: serde_json::Value,
    ) -> Result<(), RegistryError> {
        // Parse the raw status line
        let raw_status: RawStatusLine =
            serde_json::from_value(data).map_err(RegistryError::parse)?;

        // Extract PID from status line
        let status_pid = raw_status.pid;

        // Primary lookup: by PID (preferred)
        if let Some(pid) = status_pid {
            if pid != 0 {
                return self.update_or_create_by_pid(pid, session_id, raw_status);
            }
        }

        // Fallback: lookup by session_id (when no PID available)
        if let Some(&pid) = self.session_id_to_pid.get(&session_id) {
            if let Some((session, infra)) = self.sessions_by_pid.get_mut(&pid) {
                raw_status.update_session(session);
                infra.record_update();

                debug!(
                    session_id = %session_id,
                    pid = pid,
                    cost = %session.cost,
                    "Session updated from status line (by session_id)"
                );

                let view = SessionView::from_domain(session);
                let _ = self.event_publisher.send(SessionEvent::Updated {
                    session: Box::new(view),
                });
            }
            return Ok(());
        }

        // Session doesn't exist and no PID - can't create without a PID
        debug!(
            session_id = %session_id,
            "Status line without PID for unknown session, ignoring"
        );
        Ok(())
    }

    /// Updates an existing session by PID, or creates a new one.
    ///
    /// This is the core logic for status line handling with PID as primary key.
    fn update_or_create_by_pid(
        &mut self,
        pid: u32,
        session_id: SessionId,
        raw_status: RawStatusLine,
    ) -> Result<(), RegistryError> {
        if let Some((session, infra)) = self.sessions_by_pid.get_mut(&pid) {
            // Update existing session
            let old_session_id = session.id.clone();

            raw_status.update_session(session);
            infra.record_update();

            // If session_id changed (e.g., pending → real), update the index
            if old_session_id != session_id {
                // Update the session's ID
                session.id = session_id.clone();

                // Update the index
                self.session_id_to_pid.remove(&old_session_id);
                self.session_id_to_pid.insert(session_id.clone(), pid);

                info!(
                    old_id = %old_session_id,
                    new_id = %session_id,
                    pid = pid,
                    "Session ID upgraded"
                );

                // Publish removal event for old ID
                let _ = self.event_publisher.send(SessionEvent::Removed {
                    session_id: old_session_id,
                    reason: RemovalReason::Upgraded,
                });

                // Publish registered event for new ID
                let _ = self.event_publisher.send(SessionEvent::Registered {
                    session_id: session_id.clone(),
                    agent_type: session.agent_type.clone(),
                });
            }

            debug!(
                session_id = %session_id,
                pid = pid,
                cost = %session.cost,
                "Session updated from status line"
            );

            let view = SessionView::from_domain(session);
            let _ = self.event_publisher.send(SessionEvent::Updated {
                session: Box::new(view),
            });
        } else {
            // Session doesn't exist - create it
            let session = match raw_status.to_session_domain() {
                Some(s) => s,
                None => {
                    debug!(
                        session_id = %session_id,
                        pid = pid,
                        "Status line missing required fields for session creation"
                    );
                    return Ok(());
                }
            };

            // Check capacity
            if self.sessions_by_pid.len() >= MAX_SESSIONS {
                warn!(
                    session_id = %session_id,
                    "Registry full, cannot auto-register session"
                );
                return Err(RegistryError::RegistryFull { max: MAX_SESSIONS });
            }

            let agent_type = session.agent_type.clone();

            // Create infrastructure with PID
            let mut infra = SessionInfrastructure::new();
            infra.set_pid(pid);

            // Insert into storage and index
            self.sessions_by_pid.insert(pid, (session, infra));
            self.session_id_to_pid.insert(session_id.clone(), pid);

            info!(
                session_id = %session_id,
                pid = pid,
                "Session auto-registered from status line"
            );

            // Publish events
            let _ = self.event_publisher.send(SessionEvent::Registered {
                session_id: session_id.clone(),
                agent_type,
            });

            if let Some((session, _)) = self.sessions_by_pid.get(&pid) {
                let view = SessionView::from_domain(session);
                let _ = self.event_publisher.send(SessionEvent::Updated {
                    session: Box::new(view),
                });
            }
        }

        Ok(())
    }

    /// Handles applying a hook event to a session.
    ///
    /// With PID as primary key, we can look up by PID when available.
    ///
    /// Special case: SessionEnd hook immediately removes the session from the registry.
    fn handle_apply_hook_event(
        &mut self,
        session_id: SessionId,
        event_type: HookEventType,
        tool_name: Option<String>,
        notification_type: Option<String>,
        pid: Option<u32>,
        tmux_pane: Option<String>,
    ) -> Result<(), RegistryError> {
        // Handle SessionEnd specially - remove session immediately
        if event_type == HookEventType::SessionEnd {
            // Try to find the session by PID first, then by session_id
            let target_pid = pid.or_else(|| self.session_id_to_pid.get(&session_id).copied());

            if let Some(p) = target_pid {
                if self.sessions_by_pid.contains_key(&p) {
                    info!(
                        session_id = %session_id,
                        pid = p,
                        "SessionEnd hook received, removing session"
                    );
                    return self.handle_remove_by_pid(p, RemovalReason::SessionEnded);
                }
            }

            // Session doesn't exist - this is normal due to race conditions
            debug!(
                session_id = %session_id,
                "SessionEnd for non-existent session (already cleaned up or never created)"
            );
            return Ok(());
        }

        // Find session by PID first (preferred), then by session_id
        let target_pid = pid.or_else(|| self.session_id_to_pid.get(&session_id).copied());

        let (session, infra) = match target_pid.and_then(|p| self.sessions_by_pid.get_mut(&p)) {
            Some(entry) => entry,
            None => {
                // Session doesn't exist yet - this is normal due to race conditions.
                // With PID as primary key, we can create the session now if we have a PID.
                if let Some(p) = pid {
                    if p != 0 {
                        debug!(
                            session_id = %session_id,
                            pid = p,
                            event_type = ?event_type,
                            "Creating session from hook event"
                        );
                        // Create minimal session - will be updated by status line
                        use atm_core::{AgentType, Model};
                        let mut session = SessionDomain::new(
                            session_id.clone(),
                            AgentType::GeneralPurpose,
                            Model::Unknown,
                        );
                        // Set tmux pane if provided by hook
                        session.tmux_pane = tmux_pane.clone();
                        let mut infra = SessionInfrastructure::new();
                        infra.set_pid(p);

                        self.sessions_by_pid.insert(p, (session, infra));
                        self.session_id_to_pid.insert(session_id.clone(), p);

                        // Now get the entry we just created
                        if let Some((session, infra)) = self.sessions_by_pid.get_mut(&p) {
                            if event_type == HookEventType::Notification {
                                session.apply_notification(notification_type.as_deref());
                            } else {
                                session.apply_hook_event(event_type, tool_name.as_deref());
                            }
                            if let Some(ref name) = tool_name {
                                infra.record_tool_use(name, None);
                            }

                            let view = SessionView::from_domain(session);
                            let _ = self.event_publisher.send(SessionEvent::Registered {
                                session_id: session_id.clone(),
                                agent_type: session.agent_type.clone(),
                            });
                            let _ = self.event_publisher.send(SessionEvent::Updated {
                                session: Box::new(view),
                            });
                        }
                        return Ok(());
                    }
                }

                debug!(
                    session_id = %session_id,
                    event_type = ?event_type,
                    "Hook event for non-existent session without PID, ignoring"
                );
                return Ok(());
            }
        };

        // Apply the hook event to update session status
        if event_type == HookEventType::Notification {
            session.apply_notification(notification_type.as_deref());
        } else {
            session.apply_hook_event(event_type, tool_name.as_deref());
        }

        // Update tmux_pane if provided by hook (fills in for discovered sessions)
        if tmux_pane.is_some() && session.tmux_pane.is_none() {
            session.tmux_pane = tmux_pane;
        }

        debug!(
            session_id = %session.id,
            event_type = ?event_type,
            tool_name = ?tool_name,
            new_status = %session.status,
            "Hook event applied"
        );

        // Record tool usage in infrastructure
        if let Some(ref name) = tool_name {
            infra.record_tool_use(name, None);
        }

        // Publish updated event
        let view = SessionView::from_domain(session);
        let _ = self.event_publisher.send(SessionEvent::Updated {
            session: Box::new(view),
        });

        Ok(())
    }

    /// Handles getting a single session by ID.
    fn handle_get_session(&self, session_id: &SessionId) -> Option<SessionView> {
        self.session_id_to_pid
            .get(session_id)
            .and_then(|pid| self.sessions_by_pid.get(pid))
            .map(|(session, _)| SessionView::from_domain(session))
    }

    /// Handles getting all sessions.
    fn handle_get_all_sessions(&self) -> Vec<SessionView> {
        self.sessions_by_pid
            .values()
            .map(|(session, _)| SessionView::from_domain(session))
            .collect()
    }

    /// Handles removing a session by session_id.
    fn handle_remove(
        &mut self,
        session_id: SessionId,
        reason: RemovalReason,
    ) -> Result<(), RegistryError> {
        let pid = match self.session_id_to_pid.remove(&session_id) {
            Some(p) => p,
            None => return Err(RegistryError::SessionNotFound(session_id)),
        };

        self.sessions_by_pid.remove(&pid);

        info!(
            session_id = %session_id,
            pid = pid,
            reason = %reason,
            remaining_sessions = self.sessions_by_pid.len(),
            "Session removed"
        );

        // Publish removed event
        let _ = self
            .event_publisher
            .send(SessionEvent::Removed { session_id, reason });

        Ok(())
    }

    /// Handles removing a session by PID.
    fn handle_remove_by_pid(
        &mut self,
        pid: u32,
        reason: RemovalReason,
    ) -> Result<(), RegistryError> {
        let (session, _) = match self.sessions_by_pid.remove(&pid) {
            Some(entry) => entry,
            None => {
                return Err(RegistryError::SessionNotFound(SessionId::new(format!(
                    "pid-{pid}"
                ))));
            }
        };

        let session_id = session.id.clone();
        self.session_id_to_pid.remove(&session_id);

        info!(
            session_id = %session_id,
            pid = pid,
            reason = %reason,
            remaining_sessions = self.sessions_by_pid.len(),
            "Session removed"
        );

        // Publish removed event
        let _ = self
            .event_publisher
            .send(SessionEvent::Removed { session_id, reason });

        Ok(())
    }

    /// Handles cleanup of dead-process sessions.
    ///
    /// Removes sessions whose Claude Code process has terminated
    /// (PID no longer exists or was reused by a different process).
    fn handle_cleanup_stale(&mut self) {
        let now = Utc::now();

        // Collect PIDs to remove: only sessions whose process has died
        let to_remove: Vec<(u32, SessionId)> = self
            .sessions_by_pid
            .iter()
            .filter_map(|(pid, (session, infra))| {
                if !infra.is_process_alive() {
                    Some((*pid, session.id.clone()))
                } else {
                    None
                }
            })
            .collect();

        if to_remove.is_empty() {
            debug!("No dead-process sessions to clean up");
            return;
        }

        info!(count = to_remove.len(), "Cleaning up dead-process sessions");

        // Remove each session
        for (pid, session_id) in to_remove {
            // Get details for logging
            let log_details = self
                .sessions_by_pid
                .get(&pid)
                .map(|(s, _)| {
                    let secs = now.signed_duration_since(s.last_activity).num_seconds();
                    format!("last_activity={secs}s ago, pid={pid}")
                })
                .unwrap_or_default();

            self.sessions_by_pid.remove(&pid);
            self.session_id_to_pid.remove(&session_id);

            // Use warn! so it shows up without RUST_LOG=debug
            warn!(
                session_id = %session_id,
                reason = %RemovalReason::ProcessDied,
                details = %log_details,
                "Session removed by cleanup"
            );

            // Publish removed event
            let _ = self.event_publisher.send(SessionEvent::Removed {
                session_id,
                reason: RemovalReason::ProcessDied,
            });
        }
    }

    // ========================================================================
    // Accessors (for testing)
    // ========================================================================

    /// Returns the number of sessions currently registered.
    #[cfg(test)]
    pub fn session_count(&self) -> usize {
        self.sessions_by_pid.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::{AgentType, Model};
    use tokio::sync::oneshot;

    fn create_test_session(id: &str) -> SessionDomain {
        SessionDomain::new(
            SessionId::new(id),
            AgentType::GeneralPurpose,
            Model::Sonnet4,
        )
    }

    fn create_actor() -> (
        mpsc::Sender<RegistryCommand>,
        RegistryActor,
        broadcast::Receiver<SessionEvent>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let (event_tx, event_rx) = broadcast::channel(16);
        let actor = RegistryActor::new(cmd_rx, event_tx);
        (cmd_tx, actor, event_rx)
    }

    #[tokio::test]
    async fn test_register_session() {
        let (cmd_tx, mut actor, mut event_rx) = create_actor();

        let session = create_test_session("test-123");
        let (respond_tx, respond_rx) = oneshot::channel();

        cmd_tx
            .send(RegistryCommand::Register {
                session: Box::new(session),
                respond_to: respond_tx,
            })
            .await
            .unwrap();

        // Process the command manually (actor not running in background)
        if let Some(cmd) = actor.receiver.recv().await {
            actor.handle_command(cmd);
        }

        // Check response
        let result = respond_rx.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(actor.session_count(), 1);

        // Check event was published
        let event = event_rx.try_recv().unwrap();
        assert!(matches!(event, SessionEvent::Registered { .. }));
    }

    #[tokio::test]
    async fn test_register_duplicate_fails() {
        let (_, mut actor, _) = create_actor();

        let session1 = create_test_session("test-123");
        let session2 = create_test_session("test-123");

        // Register first session
        let (tx1, _) = oneshot::channel();
        let cmd1 = RegistryCommand::Register {
            session: Box::new(session1),
            respond_to: tx1,
        };
        actor.handle_command(cmd1);

        // Try to register duplicate
        let (tx2, rx2) = oneshot::channel();
        let cmd2 = RegistryCommand::Register {
            session: Box::new(session2),
            respond_to: tx2,
        };
        actor.handle_command(cmd2);

        let result = rx2.await.unwrap();
        assert!(matches!(
            result,
            Err(RegistryError::SessionAlreadyExists(_))
        ));
        assert_eq!(actor.session_count(), 1);
    }

    #[tokio::test]
    async fn test_get_session() {
        let (_, mut actor, _) = create_actor();

        // Register a session
        let session = create_test_session("test-123");
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(session),
            respond_to: tx,
        });

        // Get the session
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: SessionId::new("test-123"),
            respond_to: tx,
        });

        let result = rx.await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().id.as_str(), "test-123");
    }

    #[tokio::test]
    async fn test_get_nonexistent_session() {
        let (_, mut actor, _) = create_actor();

        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: SessionId::new("nonexistent"),
            respond_to: tx,
        });

        let result = rx.await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_all_sessions() {
        let (_, mut actor, _) = create_actor();

        // Register multiple sessions
        for i in 0..3 {
            let session = create_test_session(&format!("test-{i}"));
            let (tx, _) = oneshot::channel();
            actor.handle_command(RegistryCommand::Register {
                session: Box::new(session),
                respond_to: tx,
            });
        }

        // Get all sessions
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetAllSessions { respond_to: tx });

        let result = rx.await.unwrap();
        assert_eq!(result.len(), 3);
    }

    #[tokio::test]
    async fn test_remove_session() {
        let (_, mut actor, mut event_rx) = create_actor();

        // Register a session
        let session = create_test_session("test-123");
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(session),
            respond_to: tx,
        });

        // Drain the registered event
        let _ = event_rx.try_recv();

        // Remove the session
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::Remove {
            session_id: SessionId::new("test-123"),
            respond_to: tx,
        });

        let result = rx.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(actor.session_count(), 0);

        // Check removed event
        let event = event_rx.try_recv().unwrap();
        assert!(matches!(
            event,
            SessionEvent::Removed {
                reason: RemovalReason::Explicit,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn test_remove_nonexistent_fails() {
        let (_, mut actor, _) = create_actor();

        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::Remove {
            session_id: SessionId::new("nonexistent"),
            respond_to: tx,
        });

        let result = rx.await.unwrap();
        assert!(matches!(result, Err(RegistryError::SessionNotFound(_))));
    }

    #[tokio::test]
    async fn test_apply_hook_event() {
        let (_, mut actor, _) = create_actor();

        // Register a session
        let session = create_test_session("test-123");
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(session),
            respond_to: tx,
        });

        // Apply hook event
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyHookEvent {
            session_id: SessionId::new("test-123"),
            event_type: HookEventType::PreToolUse,
            tool_name: Some("Bash".to_string()),
            notification_type: None,
            pid: None,
            tmux_pane: None,
            respond_to: tx,
        });

        let result = rx.await.unwrap();
        assert!(result.is_ok());

        // Verify session status changed
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: SessionId::new("test-123"),
            respond_to: tx,
        });

        let view = rx.await.unwrap().unwrap();
        assert_eq!(view.status_label, "working");
        assert_eq!(view.activity_detail, Some("Bash".to_string()));
    }

    #[tokio::test]
    async fn test_apply_hook_event_session_end() {
        let (_, mut actor, mut event_rx) = create_actor();

        // Register a session
        let session = create_test_session("test-session-end");
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(session),
            respond_to: tx,
        });

        // Drain registered event
        let _ = event_rx.try_recv();

        assert_eq!(actor.session_count(), 1);

        // Apply SessionEnd hook - should remove the session
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyHookEvent {
            session_id: SessionId::new("test-session-end"),
            event_type: HookEventType::SessionEnd,
            tool_name: None,
            notification_type: None,
            pid: None,
            tmux_pane: None,
            respond_to: tx,
        });

        let result = rx.await.unwrap();
        assert!(result.is_ok());

        // Session should be removed
        assert_eq!(actor.session_count(), 0);

        // Should have received Removed event with SessionEnded reason
        let event = event_rx.try_recv().unwrap();
        assert!(matches!(
            event,
            SessionEvent::Removed {
                reason: RemovalReason::SessionEnded,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn test_apply_hook_event_session_end_nonexistent() {
        let (_, mut actor, _) = create_actor();

        // Apply SessionEnd to non-existent session (race condition scenario)
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyHookEvent {
            session_id: SessionId::new("nonexistent"),
            event_type: HookEventType::SessionEnd,
            tool_name: None,
            notification_type: None,
            pid: None,
            tmux_pane: None,
            respond_to: tx,
        });

        // Should succeed silently (not error)
        let result = rx.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_max_sessions_limit() {
        let (_, mut actor, _) = create_actor();

        // Register MAX_SESSIONS sessions
        for i in 0..MAX_SESSIONS {
            let session = create_test_session(&format!("test-{i}"));
            let (tx, _) = oneshot::channel();
            actor.handle_command(RegistryCommand::Register {
                session: Box::new(session),
                respond_to: tx,
            });
        }

        assert_eq!(actor.session_count(), MAX_SESSIONS);

        // Try to register one more
        let session = create_test_session("one-too-many");
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(session),
            respond_to: tx,
        });

        let result = rx.await.unwrap();
        assert!(matches!(
            result,
            Err(RegistryError::RegistryFull { max: MAX_SESSIONS })
        ));
        assert_eq!(actor.session_count(), MAX_SESSIONS);
    }

    #[tokio::test]
    async fn test_update_from_status_line_existing_session() {
        let (_, mut actor, _) = create_actor();

        // Register a session
        let session = create_test_session("test-123");
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(session),
            respond_to: tx,
        });

        // Update via status line
        let status_json = serde_json::json!({
            "session_id": "test-123",
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.25, "total_duration_ms": 15000},
            "context_window": {"total_input_tokens": 5000, "context_window_size": 200000}
        });

        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::UpdateFromStatusLine {
            session_id: SessionId::new("test-123"),
            data: status_json,
            respond_to: tx,
        });

        let result = rx.await.unwrap();
        assert!(result.is_ok());

        // Verify update was applied
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: SessionId::new("test-123"),
            respond_to: tx,
        });

        let view = rx.await.unwrap().unwrap();
        assert!(view.cost_display.contains("0.25") || view.cost_usd > 0.24);
    }

    #[tokio::test]
    async fn test_update_from_status_line_auto_register() {
        let (_, mut actor, mut event_rx) = create_actor();

        // Use the current process PID (a real PID that set_pid can validate)
        let current_pid = std::process::id();

        // Update for non-existent session (should auto-register)
        // Note: PID is required for auto-registration with PID-as-primary-key design
        let status_json = serde_json::json!({
            "session_id": "new-session",
            "pid": current_pid,
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.10, "total_duration_ms": 5000},
            "context_window": {"total_input_tokens": 1000, "context_window_size": 200000}
        });

        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::UpdateFromStatusLine {
            session_id: SessionId::new("new-session"),
            data: status_json,
            respond_to: tx,
        });

        let result = rx.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(actor.session_count(), 1);

        // Check registered event was published
        let event = event_rx.try_recv().unwrap();
        assert!(matches!(event, SessionEvent::Registered { .. }));
    }

    #[tokio::test]
    async fn test_cleanup_stale_no_stale_sessions() {
        let (_, mut actor, _) = create_actor();

        // Register a session (it will be fresh)
        let session = create_test_session("test-123");
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(session),
            respond_to: tx,
        });

        // Run cleanup
        actor.handle_command(RegistryCommand::CleanupStale);

        // Session should still exist (not stale)
        assert_eq!(actor.session_count(), 1);
    }

    #[tokio::test]
    async fn test_pending_session_upgrade_on_status_line() {
        let (_, mut actor, mut event_rx) = create_actor();

        // Use the current process PID (a real PID that set_pid can validate)
        let current_pid = std::process::id();

        // Register a pending session (simulating discovery without transcript)
        let pending_id = SessionId::pending_from_pid(current_pid);
        let (tx, rx) = oneshot::channel();
        let cmd = RegistryCommand::RegisterDiscovered {
            session_id: pending_id.clone(),
            pid: current_pid,
            cwd: std::path::PathBuf::from("/home/user/project"),
            tmux_pane: None,
            respond_to: tx,
        };
        actor.handle_command(cmd);
        let result = rx.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(actor.session_count(), 1);

        // Drain the registered event
        let _ = event_rx.try_recv();
        let _ = event_rx.try_recv(); // Updated event

        // Now receive a status line with the real session ID and same PID
        let status_json = serde_json::json!({
            "session_id": "real-session-uuid",
            "pid": current_pid,
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.10, "total_duration_ms": 5000},
            "context_window": {"total_input_tokens": 1000, "context_window_size": 200000}
        });

        let (tx, rx) = oneshot::channel();
        let cmd = RegistryCommand::UpdateFromStatusLine {
            session_id: SessionId::new("real-session-uuid"),
            data: status_json,
            respond_to: tx,
        };
        actor.handle_command(cmd);
        let result = rx.await.unwrap();
        assert!(result.is_ok());

        // Should still have 1 session (pending was upgraded, not a new one added)
        assert_eq!(actor.session_count(), 1);

        // The session should now have the real ID, not the pending ID
        let (tx, rx) = oneshot::channel();
        let cmd = RegistryCommand::GetSession {
            session_id: SessionId::new("real-session-uuid"),
            respond_to: tx,
        };
        actor.handle_command(cmd);
        let session = rx.await.unwrap();
        assert!(session.is_some());
        assert_eq!(session.unwrap().id.as_str(), "real-session-uuid");

        // The pending session should no longer exist
        let (tx, rx) = oneshot::channel();
        let cmd = RegistryCommand::GetSession {
            session_id: pending_id,
            respond_to: tx,
        };
        actor.handle_command(cmd);
        let pending_session = rx.await.unwrap();
        assert!(pending_session.is_none());

        // Should have received Removed event for pending and Registered for real
        let mut found_removed = false;
        let mut found_registered = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                SessionEvent::Removed {
                    reason: RemovalReason::Upgraded,
                    ..
                } => {
                    found_removed = true;
                }
                SessionEvent::Registered { session_id, .. }
                    if session_id.as_str() == "real-session-uuid" =>
                {
                    found_registered = true;
                }
                _ => {}
            }
        }
        assert!(
            found_removed,
            "Should have received Removed event with Upgraded reason"
        );
        assert!(
            found_registered,
            "Should have received Registered event for real session"
        );
    }

    #[tokio::test]
    async fn test_no_duplicate_sessions_for_same_pid() {
        // This is the key test for the fix: with PID as primary key,
        // we should never have duplicate sessions for the same Claude process.
        let (_, mut actor, _) = create_actor();

        // Use the current process PID (a real PID that set_pid can validate)
        let current_pid = std::process::id();

        // Simulate discovery finding a transcript with one session ID
        let discovered_id = SessionId::new("discovered-uuid");
        let (tx, rx) = oneshot::channel();
        let cmd = RegistryCommand::RegisterDiscovered {
            session_id: discovered_id.clone(),
            pid: current_pid,
            cwd: std::path::PathBuf::from("/home/user/project"),
            tmux_pane: None,
            respond_to: tx,
        };
        actor.handle_command(cmd);
        let result = rx.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(actor.session_count(), 1);

        // Now simulate status line arriving with a DIFFERENT session ID but SAME PID
        // (This was the bug scenario - before the fix, this would create a duplicate)
        let real_id = SessionId::new("real-uuid-from-status-line");
        let status_json = serde_json::json!({
            "session_id": "real-uuid-from-status-line",
            "pid": current_pid,
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.10, "total_duration_ms": 5000},
            "context_window": {"total_input_tokens": 1000, "context_window_size": 200000}
        });
        let (tx, rx) = oneshot::channel();
        let cmd = RegistryCommand::UpdateFromStatusLine {
            session_id: real_id.clone(),
            data: status_json,
            respond_to: tx,
        };
        actor.handle_command(cmd);
        let result = rx.await.unwrap();
        assert!(result.is_ok());

        // CRITICAL: Should still have only 1 session, not 2!
        assert_eq!(
            actor.session_count(),
            1,
            "Should have 1 session, not duplicates"
        );

        // The session should now have the real ID from the status line
        let (tx, rx) = oneshot::channel();
        let cmd = RegistryCommand::GetSession {
            session_id: real_id.clone(),
            respond_to: tx,
        };
        actor.handle_command(cmd);
        let session = rx.await.unwrap();
        assert!(session.is_some(), "Session should exist with real ID");
        assert_eq!(session.unwrap().id.as_str(), "real-uuid-from-status-line");

        // The old discovered ID should no longer exist
        let (tx, rx) = oneshot::channel();
        let cmd = RegistryCommand::GetSession {
            session_id: discovered_id,
            respond_to: tx,
        };
        actor.handle_command(cmd);
        let old_session = rx.await.unwrap();
        assert!(
            old_session.is_none(),
            "Old session ID should not exist anymore"
        );
    }
}
