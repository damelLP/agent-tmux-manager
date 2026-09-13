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
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use atm_core::{
    AgentType, LifecycleEvent, NeedsInputReason, SessionDomain, SessionId, SessionInfrastructure,
    SessionView,
};
use atm_protocol::RawStatusLine;

use super::commands::{
    LifecycleContext, RegistryCommand, RegistryError, RemovalReason, SessionEvent,
};

// ============================================================================
// Resource Limits (from RESOURCE_LIMITS.md)
// ============================================================================

/// Maximum number of sessions the registry can hold.
pub const MAX_SESSIONS: usize = 100;

// ============================================================================
// Registry Actor
// ============================================================================

/// A pending subagent awaiting correlation with a discovered session.
///
/// When a SubagentStart hook arrives, we record the parent session and agent metadata.
/// Later, when the child session registers (via discovery or hook), we correlate them.
#[derive(Debug)]
struct PendingSubagent {
    /// Session ID of the parent that spawned this subagent
    parent_session_id: SessionId,
    /// PID of the parent session (cached for ancestry check)
    parent_pid: u32,
    /// Process start time of the parent PID (to detect PID reuse)
    parent_start_time: Option<u64>,
    /// Type of agent (explore, plan, etc.)
    agent_type: AgentType,
    /// When this entry was created (for TTL cleanup)
    created_at: Instant,
}

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

    /// Pending subagent correlations awaiting child session discovery.
    /// Uses Vec for deterministic FIFO ordering — when multiple subagents
    /// are pending, the oldest match wins.
    pending_subagents: Vec<(String, PendingSubagent)>,

    /// Parent-scoped vendor ids and teammate names mapped to child session ids.
    child_refs: HashMap<(SessionId, String), SessionId>,
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
            pending_subagents: Vec::new(),
            child_refs: HashMap::new(),
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
            RegistryCommand::ApplyLifecycleEvent {
                session_id,
                event,
                harness,
                pid,
                tmux_pane,
                context,
                respond_to,
            } => {
                let result = self.handle_apply_lifecycle_event(
                    session_id, event, harness, pid, tmux_pane, context,
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
            RegistryCommand::RefreshGitInfo => {
                self.handle_refresh_git_info();
            }
            RegistryCommand::RegisterDiscovered {
                session_id,
                pid,
                cwd,
                tmux_pane,
                harness,
                respond_to,
            } => {
                let result =
                    self.handle_register_discovered(session_id, pid, cwd, tmux_pane, harness);
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

        // Resolve project/worktree if not already set
        let mut session = session;
        if session.project_root.is_none() {
            if let Some(ref cwd) = session.working_directory {
                session.project_root = atm_core::resolve_project_root(cwd);
                let (wt_path, wt_branch) = atm_core::resolve_worktree_info(cwd);
                session.worktree_path = wt_path;
                session.worktree_branch = wt_branch;
            }
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
        harness: atm_core::Harness,
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
        if let Some((existing_session, _)) = self.sessions_by_pid.get_mut(&pid) {
            if existing_session.id == session_id {
                // Same session_id, same PID — nothing to do
                debug!(
                    session_id = %session_id,
                    pid = pid,
                    "Discovered session already exists, skipping"
                );
                return Ok(());
            }

            // PID exists with a different session_id (e.g., re-discovery of an
            // upgraded session). Preserve the existing SessionDomain (cost, tokens,
            // duration, etc.) — only refresh cwd and git info from the new discovery.
            let old_id = existing_session.id.clone();
            let cwd_str = cwd.to_string_lossy().to_string();

            // Update session_id to match the new discovery
            existing_session.id = session_id.clone();

            existing_session.working_directory = Some(cwd_str.clone());
            existing_session.project_root = atm_core::resolve_project_root(&cwd_str);
            let (wt_path, wt_branch) = atm_core::resolve_worktree_info(&cwd_str);
            existing_session.worktree_path = wt_path;
            existing_session.worktree_branch = wt_branch;
            if tmux_pane.is_some() {
                existing_session.tmux_pane = tmux_pane;
            }

            info!(
                old_id = %old_id,
                new_id = %session_id,
                pid = pid,
                "Re-discovered existing session, refreshed git info (metadata preserved)"
            );

            let view = SessionView::from_domain(existing_session);
            let _ = self.event_publisher.send(SessionEvent::Updated {
                session: Box::new(view),
            });

            // Update the session_id index
            self.session_id_to_pid.remove(&old_id);
            self.session_id_to_pid.insert(session_id, pid);

            return Ok(());
        }

        // Check capacity
        if self.sessions_by_pid.len() >= MAX_SESSIONS {
            warn!(
                session_id = %session_id,
                current = self.sessions_by_pid.len(),
                max = MAX_SESSIONS,
                "Registry is full, cannot register discovered session"
            );
            return Err(RegistryError::RegistryFull { max: MAX_SESSIONS });
        }

        // Create minimal session with defaults (genuinely new process).
        // Agent type and model will be updated when the status line arrives.
        // Harness tag comes from whichever discoverer matched (Claude, pi,
        // future); subsequent adapter events can refine but not change identity.
        use atm_core::Model;
        let session = build_session_from_pid(
            session_id.clone(),
            AgentType::GeneralPurpose,
            Model::Unknown,
            harness,
            tmux_pane,
            Some(cwd),
        );
        let agent_type = session.agent_type.clone();

        // Create new infrastructure with PID
        let mut infra = SessionInfrastructure::new();
        infra.set_pid(pid);

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

        // Try to correlate with pending subagent
        self.try_correlate_subagent(&session_id, pid);

        Ok(())
    }

    /// Renames an existing session at `pid` from `old_id` to `new_id`,
    /// updates the session_id index, and publishes the
    /// `Removed{Upgraded}` + `Registered` event pair so TUI subscribers
    /// see the id transition cleanly.
    ///
    /// Used by both the Claude status-line path and any vendor adapter
    /// (pi today) whose first events arrive before the real session_id
    /// is known. Without this, a session discovered as `pending-{pid}`
    /// stays at that id forever even after real adapter events fire.
    ///
    /// No-op if the session is not present at `pid`.
    fn reconcile_session_id(&mut self, pid: u32, old_id: SessionId, new_id: SessionId) {
        if old_id == new_id {
            return;
        }
        let Some((session, _)) = self.sessions_by_pid.get_mut(&pid) else {
            return;
        };
        session.id = new_id.clone();
        let agent_type = session.agent_type.clone();

        self.session_id_to_pid.remove(&old_id);
        self.session_id_to_pid.insert(new_id.clone(), pid);

        info!(
            old_id = %old_id,
            new_id = %new_id,
            pid = pid,
            "Session ID upgraded"
        );

        let _ = self.event_publisher.send(SessionEvent::Removed {
            session_id: old_id,
            reason: RemovalReason::Upgraded,
        });
        let _ = self.event_publisher.send(SessionEvent::Registered {
            session_id: new_id,
            agent_type,
        });
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
                let cwd_changed = raw_status.update_session(session);
                infra.record_update();

                // Resolve project/worktree if not yet set, or if cwd changed
                if session.project_root.is_none() || cwd_changed {
                    if let Some(ref cwd) = session.working_directory {
                        if cwd_changed {
                            info!(
                                session_id = %session_id,
                                pid = pid,
                                new_cwd = %cwd,
                                "Working directory changed, re-resolving git info"
                            );
                        }
                        session.project_root = atm_core::resolve_project_root(cwd);
                        let (wt_path, wt_branch) = atm_core::resolve_worktree_info(cwd);
                        session.worktree_path = wt_path;
                        session.worktree_branch = wt_branch;
                    }
                }

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
        // If a session already exists at this PID with a different id
        // (e.g. pending → real), reconcile *before* taking the mutable
        // borrow below so the helper can access `self` cleanly. The
        // Updated event still fires after with the renamed session.
        if let Some((existing, _)) = self.sessions_by_pid.get(&pid) {
            let current_id = existing.id.clone();
            if current_id != session_id {
                self.reconcile_session_id(pid, current_id, session_id.clone());
            }
        }

        if let Some((session, infra)) = self.sessions_by_pid.get_mut(&pid) {
            let cwd_changed = raw_status.update_session(session);
            infra.record_update();

            // Resolve project/worktree if not yet set, or if cwd changed
            if session.project_root.is_none() || cwd_changed {
                if let Some(ref cwd) = session.working_directory {
                    if cwd_changed {
                        info!(
                            session_id = %session.id,
                            pid = pid,
                            new_cwd = %cwd,
                            "Working directory changed, re-resolving git info"
                        );
                    }
                    session.project_root = atm_core::resolve_project_root(cwd);
                    let (wt_path, wt_branch) = atm_core::resolve_worktree_info(cwd);
                    session.worktree_path = wt_path;
                    session.worktree_branch = wt_branch;
                }
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
            let mut session = match raw_status.to_session_domain() {
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

            // Resolve project/worktree from working directory
            if let Some(ref cwd) = session.working_directory {
                session.project_root = atm_core::resolve_project_root(cwd);
                let (wt_path, wt_branch) = atm_core::resolve_worktree_info(cwd);
                session.worktree_path = wt_path;
                session.worktree_branch = wt_branch;
            }

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

    /// Handles applying a vendor-neutral lifecycle event to a session.
    ///
    /// With PID as primary key, we can look up by PID when available.
    ///
    /// Special cases:
    /// - `SessionEnd` immediately removes the session from the registry.
    /// - `ChildSessionStart`/`ChildSessionEnd` track subagent correlation.
    fn handle_apply_lifecycle_event(
        &mut self,
        session_id: SessionId,
        event: LifecycleEvent,
        harness: atm_core::Harness,
        pid: Option<u32>,
        tmux_pane: Option<String>,
        context: LifecycleContext,
    ) -> Result<(), RegistryError> {
        let target_pid = pid.or_else(|| self.session_id_to_pid.get(&session_id).copied());

        // SessionEnd: remove session immediately.
        if matches!(event, LifecycleEvent::SessionEnd { .. }) {
            if let Some(p) = target_pid {
                if self.sessions_by_pid.contains_key(&p) {
                    info!(
                        session_id = %session_id,
                        pid = p,
                        "SessionEnd received, removing session"
                    );
                    return self.handle_remove_by_pid(p, RemovalReason::SessionEnded);
                }
            }

            debug!(
                session_id = %session_id,
                "SessionEnd for non-existent session (already cleaned up or never created)"
            );
            return Ok(());
        }

        let tool_name = tool_name_from_event(&event);

        // Pending → real upgrade: a session discovered via /proc starts
        // life as `pending-{pid}`. The first vendor-adapter event with
        // a real session_id is our signal to reconcile, mirroring the
        // Claude status-line upgrade path. Limited to pending → real
        // so that ordinary pi events (which carry session_id on every
        // frame) don't thrash the index.
        if let Some(p) = target_pid {
            if let Some((existing, _)) = self.sessions_by_pid.get(&p) {
                let current_id = existing.id.clone();
                if current_id.is_pending() && !session_id.is_pending() && current_id != session_id {
                    self.reconcile_session_id(p, current_id, session_id.clone());
                }
            }
        }

        if let Some(parent_pid) = target_pid.filter(|pid| self.sessions_by_pid.contains_key(pid)) {
            if self.prepare_child_event(parent_pid, &event, harness, &context) {
                return Ok(());
            }
        }

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
                            event = ?event,
                            "Creating session from lifecycle event"
                        );
                        // Read cwd from /proc/{pid}/cwd so a session created
                        // via a vendor adapter event lands grouped under the
                        // right project / branch from frame one. Without this,
                        // lifecycle-event-created sessions fall into the
                        // "Other" tree bucket because working_directory is None.
                        let proc_cwd = std::fs::read_link(format!("/proc/{p}/cwd")).ok();
                        use atm_core::Model;
                        let session = build_session_from_pid(
                            session_id.clone(),
                            AgentType::GeneralPurpose,
                            Model::Unknown,
                            harness,
                            tmux_pane.clone(),
                            proc_cwd,
                        );

                        let mut infra = SessionInfrastructure::new();
                        infra.set_pid(p);

                        self.sessions_by_pid.insert(p, (session, infra));
                        self.session_id_to_pid.insert(session_id.clone(), p);

                        let routed = self.prepare_child_event(p, &event, harness, &context);
                        if let Some((session, infra)) = self.sessions_by_pid.get_mut(&p) {
                            if !routed {
                                session.apply_lifecycle_event(&event);
                                session.set_first_prompt_from_event(&event);
                                if let Some(name) = tool_name.as_deref() {
                                    infra.record_tool_use(name, None);
                                }
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

                        if !routed {
                            self.apply_background_activity(p, context.background_activity);
                        }
                        self.try_correlate_subagent(&session_id, p);

                        return Ok(());
                    }
                }

                debug!(
                    session_id = %session_id,
                    event = ?event,
                    "Lifecycle event for non-existent session without PID, ignoring"
                );
                return Ok(());
            }
        };

        session.apply_lifecycle_event(&event);
        session.set_first_prompt_from_event(&event);

        if tmux_pane.is_some() && session.tmux_pane.is_none() {
            session.tmux_pane = tmux_pane;
        }

        debug!(
            session_id = %session.id,
            event = ?event,
            new_status = %session.status,
            "Lifecycle event applied"
        );

        if let Some(name) = tool_name.as_deref() {
            infra.record_tool_use(name, None);
        }

        let view = SessionView::from_domain(session);
        let _ = self.event_publisher.send(SessionEvent::Updated {
            session: Box::new(view),
        });

        if let Some(pid) = target_pid {
            self.apply_background_activity(pid, context.background_activity);
        }
        Ok(())
    }

    fn prepare_child_event(
        &mut self,
        parent_pid: u32,
        event: &LifecycleEvent,
        harness: atm_core::Harness,
        context: &LifecycleContext,
    ) -> bool {
        let Some(parent_id) = self
            .sessions_by_pid
            .get(&parent_pid)
            .map(|(parent, _)| parent.id.clone())
        else {
            return false;
        };
        if let Some((name, agent_id)) = &context.child_alias {
            self.register_child_alias(&parent_id, name, agent_id);
        }
        match event {
            LifecycleEvent::ChildSessionStart {
                id: Some(agent_id),
                role,
            } => {
                self.pending_subagents.push((
                    agent_id.clone(),
                    PendingSubagent {
                        parent_session_id: parent_id.clone(),
                        parent_pid,
                        parent_start_time: crate::tmux::get_process_start_time(parent_pid),
                        agent_type: child_agent_type(role.as_deref()),
                        created_at: Instant::now(),
                    },
                ));
                let child_id = self.child_id_for(&parent_id, agent_id, false);
                if let Some(pid) =
                    self.ensure_child_session(parent_pid, &child_id, role.as_deref(), harness)
                {
                    if let Some((child, _)) = self.sessions_by_pid.get(&pid) {
                        self.child_refs
                            .insert((parent_id.clone(), agent_id.clone()), child.id.clone());
                    }
                }
            }
            LifecycleEvent::ChildSessionEnd { id: Some(agent_id) } => {
                self.pending_subagents.retain(|(id, _)| id != agent_id);
                let child_id = self.child_id_for(&parent_id, agent_id, false);
                self.remove_child(&parent_id, &child_id, RemovalReason::SessionEnded);
            }
            _ => {}
        }
        if context.child_id.is_some() || context.child_name.is_some() {
            self.route_child_event(parent_pid, context, event, harness);
            true
        } else {
            false
        }
    }

    fn apply_background_activity(&mut self, pid: u32, activity: Option<(u32, u32)>) {
        if let Some((running, scheduled)) = activity {
            self.set_background_activity(pid, running, scheduled);
            if running == 0 {
                self.sweep_children(pid);
            }
        }
    }

    fn child_id_for(&self, parent: &SessionId, reference: &str, is_name: bool) -> SessionId {
        self.child_refs
            .get(&(parent.clone(), reference.to_string()))
            .cloned()
            .unwrap_or_else(|| {
                SessionId::new(if is_name {
                    format!("{reference}@{parent}")
                } else {
                    reference.to_string()
                })
            })
    }

    fn child_pid(&self, parent: &SessionId, child_id: &SessionId) -> Option<u32> {
        self.session_id_to_pid
            .get(child_id)
            .copied()
            .filter(|pid| {
                self.sessions_by_pid
                    .get(pid)
                    .is_some_and(|(child, _)| child.parent_session_id.as_ref() == Some(parent))
            })
            .or_else(|| {
                self.sessions_by_pid.iter().find_map(|(pid, (child, _))| {
                    (child.id == *child_id && child.parent_session_id.as_ref() == Some(parent))
                        .then_some(*pid)
                })
            })
    }

    fn ensure_child_session(
        &mut self,
        parent_pid: u32,
        child_id: &SessionId,
        role: Option<&str>,
        harness: atm_core::Harness,
    ) -> Option<u32> {
        let parent_id = self
            .sessions_by_pid
            .get(&parent_pid)
            .map(|(parent, _)| parent.id.clone())?;
        if let Some(pid) = self.session_id_to_pid.get(child_id).copied() {
            return self.child_pid(&parent_id, child_id).map(|_| pid);
        }
        if self.sessions_by_pid.len() >= MAX_SESSIONS {
            return None;
        }

        let mut child = {
            let (parent, _) = self.sessions_by_pid.get(&parent_pid)?;
            let mut child =
                SessionDomain::new(child_id.clone(), child_agent_type(role), parent.model);
            child.harness = harness;
            child.model_display_override = parent.model_display_override.clone();
            child.tmux_pane = parent.tmux_pane.clone();
            child.working_directory = parent.working_directory.clone();
            child.project_root = parent.project_root.clone();
            child.worktree_path = parent.worktree_path.clone();
            child.worktree_branch = parent.worktree_branch.clone();
            child.parent_session_id = Some(parent_id.clone());
            child
        };
        child.apply_lifecycle_event(&LifecycleEvent::WorkingStart);
        let child_pid = self.generate_synthetic_pid();
        let agent_type = child.agent_type.clone();
        let child_view = SessionView::from_domain(&child);
        self.sessions_by_pid
            .insert(child_pid, (child, SessionInfrastructure::new()));
        self.session_id_to_pid.insert(child_id.clone(), child_pid);

        let parent_view = self
            .sessions_by_pid
            .get_mut(&parent_pid)
            .map(|(parent, _)| {
                if !parent.child_session_ids.contains(child_id) {
                    parent.child_session_ids.push(child_id.clone());
                }
                SessionView::from_domain(parent)
            });
        let _ = self.event_publisher.send(SessionEvent::Registered {
            session_id: child_id.clone(),
            agent_type,
        });
        let _ = self.event_publisher.send(SessionEvent::Updated {
            session: Box::new(child_view),
        });
        if let Some(session) = parent_view {
            let _ = self.event_publisher.send(SessionEvent::Updated {
                session: Box::new(session),
            });
        }
        Some(child_pid)
    }

    fn route_child_event(
        &mut self,
        parent_pid: u32,
        context: &LifecycleContext,
        event: &LifecycleEvent,
        harness: atm_core::Harness,
    ) {
        let Some(parent_id) = self
            .sessions_by_pid
            .get(&parent_pid)
            .map(|(parent, _)| parent.id.clone())
        else {
            return;
        };
        let (reference, is_name) = match (&context.child_id, &context.child_name) {
            (Some(id), _) => (id, false),
            (None, Some(name)) => (name, true),
            (None, None) => return,
        };
        let child_id = self.child_id_for(&parent_id, reference, is_name);
        let Some(child_pid) = self.ensure_child_session(
            parent_pid,
            &child_id,
            context.child_role.as_deref(),
            harness,
        ) else {
            return;
        };
        let actual_id = self
            .sessions_by_pid
            .get(&child_pid)
            .map(|(child, _)| child.id.clone())
            .unwrap_or(child_id);
        for reference in [&context.child_id, &context.child_name]
            .into_iter()
            .flatten()
        {
            self.child_refs
                .insert((parent_id.clone(), reference.clone()), actual_id.clone());
        }
        self.apply_to_session(child_pid, event, None);
    }

    fn register_child_alias(&mut self, parent: &SessionId, name: &str, agent_id: &str) {
        let name = name.trim();
        let agent_id = agent_id.trim();
        if name.is_empty() || agent_id.is_empty() {
            return;
        }
        let named = self
            .child_refs
            .get(&(parent.clone(), name.to_string()))
            .cloned();
        let identified = self
            .child_refs
            .get(&(parent.clone(), agent_id.to_string()))
            .cloned()
            .or_else(|| {
                let id = SessionId::new(agent_id);
                self.child_pid(parent, &id).map(|_| id)
            });
        let target = identified
            .clone()
            .or_else(|| named.clone())
            .unwrap_or_else(|| SessionId::new(agent_id));

        if let Some(stale) = named.filter(|id| id != &target) {
            if let Some(pid) = self.child_pid(parent, &stale) {
                let synthetic = self
                    .sessions_by_pid
                    .get(&pid)
                    .is_some_and(|(_, infra)| infra.pid.is_none());
                if synthetic {
                    self.remove_child(parent, &stale, RemovalReason::Upgraded);
                }
            }
        }
        self.child_refs
            .insert((parent.clone(), name.to_string()), target.clone());
        self.child_refs
            .insert((parent.clone(), agent_id.to_string()), target);
    }

    fn remove_child(&mut self, parent: &SessionId, child_id: &SessionId, reason: RemovalReason) {
        if let Some(pid) = self.child_pid(parent, child_id) {
            let _ = self.handle_remove_by_pid(pid, reason);
        }
    }

    fn apply_to_session(&mut self, pid: u32, event: &LifecycleEvent, tmux_pane: Option<String>) {
        let Some((session, infra)) = self.sessions_by_pid.get_mut(&pid) else {
            return;
        };
        session.apply_lifecycle_event(event);
        session.set_first_prompt_from_event(event);
        if tmux_pane.is_some() && session.tmux_pane.is_none() {
            session.tmux_pane = tmux_pane;
        }
        if let Some(name) = tool_name_from_event(event) {
            infra.record_tool_use(&name, None);
        }
        let _ = self.event_publisher.send(SessionEvent::Updated {
            session: Box::new(SessionView::from_domain(session)),
        });
    }

    fn sweep_children(&mut self, parent_pid: u32) {
        let Some((parent_id, child_ids)) = self
            .sessions_by_pid
            .get(&parent_pid)
            .map(|(parent, _)| (parent.id.clone(), parent.child_session_ids.clone()))
        else {
            return;
        };
        let stale: Vec<_> = child_ids
            .into_iter()
            .filter(|id| {
                self.child_pid(&parent_id, id).is_some_and(|pid| {
                    self.sessions_by_pid
                        .get(&pid)
                        .is_some_and(|(child, infra)| {
                            infra.pid.is_none() && child.status == atm_core::SessionStatus::Working
                        })
                })
            })
            .collect();
        for child_id in stale {
            self.remove_child(&parent_id, &child_id, RemovalReason::SessionEnded);
        }
    }

    fn set_background_activity(&mut self, pid: u32, running: u32, scheduled: u32) {
        let Some((session, _)) = self.sessions_by_pid.get_mut(&pid) else {
            return;
        };
        let mut parts = Vec::with_capacity(2);
        if running > 0 {
            parts.push(format!(
                "{running} bg task{}",
                if running == 1 { "" } else { "s" }
            ));
        }
        if scheduled > 0 {
            parts.push(format!("{scheduled} scheduled"));
        }
        if !parts.is_empty() && session.status == atm_core::SessionStatus::Idle {
            session.current_activity =
                Some(atm_core::ActivityDetail::with_context(&parts.join(", ")));
            let _ = self.event_publisher.send(SessionEvent::Updated {
                session: Box::new(SessionView::from_domain(session)),
            });
        }
    }

    fn detach_removed_session(&mut self, session: &SessionDomain, reason: RemovalReason) {
        let children: Vec<_> = session
            .child_session_ids
            .iter()
            .filter_map(|id| {
                self.session_id_to_pid.get(id).copied().map(|pid| {
                    let synthetic = self
                        .sessions_by_pid
                        .get(&pid)
                        .is_some_and(|(_, infra)| infra.pid.is_none());
                    (id.clone(), pid, synthetic)
                })
            })
            .collect();
        for (id, pid, synthetic) in children {
            if synthetic {
                self.sessions_by_pid.remove(&pid);
                self.session_id_to_pid.remove(&id);
                let _ = self.event_publisher.send(SessionEvent::Removed {
                    session_id: id,
                    reason,
                });
            } else if let Some((child, _)) = self.sessions_by_pid.get_mut(&pid) {
                child.parent_session_id = None;
                let _ = self.event_publisher.send(SessionEvent::Updated {
                    session: Box::new(SessionView::from_domain(child)),
                });
            }
        }

        if let Some(parent_pid) = session
            .parent_session_id
            .as_ref()
            .and_then(|id| self.session_id_to_pid.get(id))
            .copied()
        {
            if let Some((parent, _)) = self.sessions_by_pid.get_mut(&parent_pid) {
                parent.child_session_ids.retain(|id| id != &session.id);
                let _ = self.event_publisher.send(SessionEvent::Updated {
                    session: Box::new(SessionView::from_domain(parent)),
                });
            }
        }
        self.child_refs
            .retain(|(parent, _), child| parent != &session.id && child != &session.id);
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

        if let Some((session, _)) = self.sessions_by_pid.remove(&pid) {
            self.detach_removed_session(&session, reason);
        }

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
        self.detach_removed_session(&session, reason);

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

    /// Attempts to correlate a newly registered session with a pending subagent.
    ///
    /// Uses PID ancestry to check if the new session's process is a child of
    /// a known parent session's process. If matched, links parent and child
    /// session IDs and removes the pending entry.
    ///
    /// # Blocking I/O
    ///
    /// Calls `is_descendant_of` which reads `/proc/{pid}/stat` (up to 20 times).
    /// These are pseudo-filesystem reads served from kernel memory (~1μs each),
    /// well under Tokio's acceptable sync threshold. If this proves problematic
    /// on exotic filesystems, move resolution to `spawn_blocking`.
    fn try_correlate_subagent(&mut self, session_id: &SessionId, pid: u32) {
        // Find matching pending subagent (FIFO order — Vec guarantees oldest-first)
        let matched_index = self.pending_subagents.iter().position(|(_, pending)| {
            if pending.created_at.elapsed() >= Duration::from_secs(30) || pending.parent_pid == 0 {
                return false;
            }
            // Verify the parent PID hasn't been reused by checking start time
            let start_time_matches = match pending.parent_start_time {
                Some(expected) => {
                    crate::tmux::get_process_start_time(pending.parent_pid) == Some(expected)
                }
                // If we couldn't capture start time originally, skip reuse check
                None => true,
            };
            start_time_matches && is_descendant_of(pid, pending.parent_pid)
        });

        if let Some(index) = matched_index {
            let (agent_id, pending) = self.pending_subagents.remove(index);

            info!(
                child_session_id = %session_id,
                parent_session_id = %pending.parent_session_id,
                agent_id = %agent_id,
                agent_type = %pending.agent_type,
                "Correlated subagent with discovered session"
            );

            let placeholder_id = self.child_id_for(&pending.parent_session_id, &agent_id, false);
            if let Some(placeholder_pid) =
                self.child_pid(&pending.parent_session_id, &placeholder_id)
            {
                let synthetic = self
                    .sessions_by_pid
                    .get(&placeholder_pid)
                    .is_some_and(|(_, infra)| infra.pid.is_none());
                if synthetic && placeholder_pid != pid {
                    self.sessions_by_pid.remove(&placeholder_pid);
                    self.session_id_to_pid.remove(&placeholder_id);
                    let _ = self.event_publisher.send(SessionEvent::Removed {
                        session_id: placeholder_id.clone(),
                        reason: RemovalReason::Upgraded,
                    });
                }
            }
            self.session_id_to_pid.insert(session_id.clone(), pid);
            for ((parent, _), child) in &mut self.child_refs {
                if parent == &pending.parent_session_id && child == &placeholder_id {
                    *child = session_id.clone();
                }
            }
            self.child_refs.insert(
                (pending.parent_session_id.clone(), agent_id),
                session_id.clone(),
            );

            // Link parent to child
            if let Some((parent_session, _)) = self.sessions_by_pid.get_mut(&pending.parent_pid) {
                parent_session
                    .child_session_ids
                    .retain(|id| id != &placeholder_id);
                if !parent_session.child_session_ids.contains(session_id) {
                    parent_session.child_session_ids.push(session_id.clone());
                }
            }

            // Link child to parent (move, no clone — pending is owned)
            if let Some((child_session, _)) = self.sessions_by_pid.get_mut(&pid) {
                child_session.parent_session_id = Some(pending.parent_session_id);
                child_session.agent_type = pending.agent_type;
            }
        }
    }

    /// Handles cleanup of dead-process sessions.
    ///
    /// Removes sessions whose Claude Code process has terminated
    /// (PID no longer exists or was reused by a different process).
    fn handle_cleanup_stale(&mut self) {
        // Clean up expired pending subagent correlations
        self.pending_subagents
            .retain(|(_, p)| p.created_at.elapsed() < Duration::from_secs(30));

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

            let removed = self.sessions_by_pid.remove(&pid);
            self.session_id_to_pid.remove(&session_id);
            if let Some((session, _)) = removed {
                self.detach_removed_session(&session, RemovalReason::ProcessDied);
            }

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

    /// Refreshes git info (branch, worktree) for all sessions.
    ///
    /// Detects branch switches that happen without a working directory change
    /// (e.g., `git checkout other-branch` in the same directory).
    fn handle_refresh_git_info(&mut self) {
        let mut updated_count = 0;

        for (pid, (session, _)) in self.sessions_by_pid.iter_mut() {
            let cwd = match &session.working_directory {
                Some(cwd) => cwd.clone(),
                None => continue,
            };

            let new_project_root = atm_core::resolve_project_root(&cwd);
            let (new_wt_path, new_wt_branch) = atm_core::resolve_worktree_info(&cwd);

            let changed = session.project_root != new_project_root
                || session.worktree_path != new_wt_path
                || session.worktree_branch != new_wt_branch;

            if changed {
                info!(
                    session_id = %session.id,
                    pid = pid,
                    old_branch = ?session.worktree_branch,
                    new_branch = ?new_wt_branch,
                    "Git info changed, updating session"
                );
                session.project_root = new_project_root;
                session.worktree_path = new_wt_path;
                session.worktree_branch = new_wt_branch;
                updated_count += 1;

                let view = SessionView::from_domain(session);
                let _ = self.event_publisher.send(SessionEvent::Updated {
                    session: Box::new(view),
                });
            }
        }

        if updated_count > 0 {
            info!(updated_count, "Git info refresh completed with changes");
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

    /// Returns the number of pending subagent correlations (for testing).
    #[cfg(test)]
    pub fn pending_subagent_count(&self) -> usize {
        self.pending_subagents.len()
    }
}

/// Builds a fresh `SessionDomain` for a newly-observed PID and resolves
/// cwd-derived fields (project root, worktree info, working directory).
///
/// Used by both the /proc-discovery path (`handle_register_discovered`)
/// and the create-on-event path inside `handle_apply_lifecycle_event` so
/// that sessions land grouped under the right project / branch from frame
/// one regardless of how they were first observed.
///
/// When `cwd` is `None` (e.g., `/proc/{pid}/cwd` read failed) the
/// project/worktree/working_directory fields are left at their defaults.
fn build_session_from_pid(
    session_id: SessionId,
    agent_type: AgentType,
    model: atm_core::Model,
    harness: atm_core::Harness,
    tmux_pane: Option<String>,
    cwd: Option<PathBuf>,
) -> SessionDomain {
    let mut session = SessionDomain::new(session_id, agent_type, model);
    session.harness = harness;
    session.tmux_pane = tmux_pane;
    if let Some(cwd) = cwd {
        // Note: resolve_* are local stat() calls walking up ~5 dirs (~5μs),
        // acceptable inline per Tokio guidelines for sub-100μs sync work.
        let cwd_str = cwd.to_string_lossy().to_string();
        session.project_root = atm_core::resolve_project_root(&cwd_str);
        let (wt_path, wt_branch) = atm_core::resolve_worktree_info(&cwd_str);
        session.worktree_path = wt_path;
        session.worktree_branch = wt_branch;
        session.working_directory = Some(cwd_str);
    }
    session
}

fn child_agent_type(role: Option<&str>) -> AgentType {
    match role.map(str::trim).filter(|role| !role.is_empty()) {
        Some(role) => match AgentType::from_subagent_type(role) {
            AgentType::GeneralPurpose => AgentType::Custom(role.to_string()),
            agent_type => agent_type,
        },
        None => AgentType::Custom("subagent".to_string()),
    }
}

/// Extracts a tool name from a `LifecycleEvent`, when present.
///
/// Used to record tool usage on the session's infrastructure record.
fn tool_name_from_event(event: &LifecycleEvent) -> Option<String> {
    match event {
        LifecycleEvent::ToolCallStart { name, .. } | LifecycleEvent::ToolCallEnd { name, .. } => {
            Some(name.as_str().to_string())
        }
        LifecycleEvent::NeedsInput { reason } => match reason {
            NeedsInputReason::InteractiveTool { tool }
            | NeedsInputReason::PermissionGate { tool } => Some(tool.as_str().to_string()),
            NeedsInputReason::Notification { .. } => None,
        },
        _ => None,
    }
}

/// Check if `pid` is a descendant of `ancestor_pid` by walking /proc.
///
/// Walks up the process tree via parent PID lookups, with a max depth
/// of 20 to prevent infinite loops in case of circular references.
fn is_descendant_of(pid: u32, ancestor_pid: u32) -> bool {
    let mut current = pid;
    for _ in 0..20 {
        if current == ancestor_pid {
            return true;
        }
        if current <= 1 {
            return false;
        }
        match crate::tmux::get_parent_pid(current) {
            Some(ppid) => current = ppid,
            None => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::{AgentType, Model, Tool};
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

        // Apply lifecycle event
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
            session_id: SessionId::new("test-123"),
            event: LifecycleEvent::ToolCallStart {
                name: Tool::Bash,
                tool_use_id: None,
                input: None,
            },
            harness: atm_core::Harness::Unknown,
            pid: None,
            tmux_pane: None,
            context: LifecycleContext::default(),
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
        actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
            session_id: SessionId::new("test-session-end"),
            event: LifecycleEvent::SessionEnd { reason: None },
            harness: atm_core::Harness::Unknown,
            pid: None,
            tmux_pane: None,
            context: LifecycleContext::default(),
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
        actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
            session_id: SessionId::new("nonexistent"),
            event: LifecycleEvent::SessionEnd { reason: None },
            harness: atm_core::Harness::Unknown,
            pid: None,
            tmux_pane: None,
            context: LifecycleContext::default(),
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
            harness: atm_core::Harness::Unknown,
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
    async fn test_pending_session_upgrade_on_lifecycle_event() {
        // Mirrors test_pending_session_upgrade_on_status_line but
        // exercises the vendor-adapter lifecycle path. Without the
        // reconcile call in handle_apply_lifecycle_event, a pi
        // session discovered via /proc would stay as `pending-{pid}`
        // forever, even after the first event with a real session_id.
        let (_, mut actor, mut event_rx) = create_actor();

        let current_pid = std::process::id();
        let pending_id = SessionId::pending_from_pid(current_pid);

        // Step 1: discovery registers a pending session.
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::RegisterDiscovered {
            session_id: pending_id.clone(),
            pid: current_pid,
            cwd: std::path::PathBuf::from("/home/user/project"),
            tmux_pane: None,
            harness: atm_core::Harness::Pi,
            respond_to: tx,
        });
        rx.await.unwrap().unwrap();
        assert_eq!(actor.session_count(), 1);

        // Drain register/update events from discovery.
        while event_rx.try_recv().is_ok() {}

        // Step 2: a vendor adapter (pi) fires a lifecycle event with
        // the real session id and the same PID. The session should
        // get its id upgraded.
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
            session_id: SessionId::new("real-pi-session"),
            event: atm_core::LifecycleEvent::WorkingStart,
            harness: atm_core::Harness::Pi,
            pid: Some(current_pid),
            tmux_pane: None,
            context: LifecycleContext::default(),
            respond_to: tx,
        });
        rx.await.unwrap().unwrap();

        // Still one session (upgraded, not added).
        assert_eq!(actor.session_count(), 1);

        // Lookup by real id succeeds.
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: SessionId::new("real-pi-session"),
            respond_to: tx,
        });
        let session = rx.await.unwrap();
        assert!(session.is_some(), "real id should resolve");
        assert_eq!(session.unwrap().id.as_str(), "real-pi-session");

        // Lookup by old pending id fails.
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: pending_id,
            respond_to: tx,
        });
        assert!(
            rx.await.unwrap().is_none(),
            "pending id should no longer resolve"
        );

        // The Removed{Upgraded} + Registered event pair should fire.
        let mut found_removed = false;
        let mut found_registered = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                SessionEvent::Removed {
                    reason: RemovalReason::Upgraded,
                    ..
                } => found_removed = true,
                SessionEvent::Registered { session_id, .. }
                    if session_id.as_str() == "real-pi-session" =>
                {
                    found_registered = true;
                }
                _ => {}
            }
        }
        assert!(found_removed, "expected Removed{{Upgraded}} for pending id");
        assert!(found_registered, "expected Registered for real id");
    }

    #[tokio::test]
    async fn test_lifecycle_event_does_not_rename_real_to_real() {
        // Defensive: only pending → real triggers reconcile. Two
        // adapter events with different real session_ids on the same
        // PID must NOT thrash the index — that would mean some other
        // bug (PID reuse, stale state) and the daemon shouldn't paper
        // over it by silently renaming.
        let (_, mut actor, _) = create_actor();
        let current_pid = std::process::id();

        // Seed a session with a real (non-pending) id via discovery.
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::RegisterDiscovered {
            session_id: SessionId::new("first-real-id"),
            pid: current_pid,
            cwd: std::path::PathBuf::from("/tmp"),
            tmux_pane: None,
            harness: atm_core::Harness::Pi,
            respond_to: tx,
        });
        rx.await.unwrap().unwrap();

        // Apply a lifecycle event with a *different* real id.
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
            session_id: SessionId::new("different-real-id"),
            event: atm_core::LifecycleEvent::WorkingStart,
            harness: atm_core::Harness::Pi,
            pid: Some(current_pid),
            tmux_pane: None,
            context: LifecycleContext::default(),
            respond_to: tx,
        });
        let _ = rx.await.unwrap();

        // Original id still resolves; rename did not occur.
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: SessionId::new("first-real-id"),
            respond_to: tx,
        });
        assert!(
            rx.await.unwrap().is_some(),
            "real id must not be renamed by another real id"
        );
    }

    #[tokio::test]
    async fn test_subagent_start_records_pending() {
        let (_, mut actor, _) = create_actor();

        // Register a parent session
        let session = create_test_session("parent-session");
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(session),
            respond_to: tx,
        });

        assert_eq!(actor.pending_subagent_count(), 0);

        // Send SubagentStart hook event with agent_id
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
            session_id: SessionId::new("parent-session"),
            event: LifecycleEvent::ChildSessionStart {
                id: Some("agent-abc-123".into()),
                role: Some("explore".into()),
            },
            harness: atm_core::Harness::Unknown,
            pid: None,
            tmux_pane: None,
            context: LifecycleContext::default(),
            respond_to: tx,
        });

        let result = rx.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(actor.pending_subagent_count(), 1);
    }

    #[tokio::test]
    async fn test_subagent_stop_clears_pending() {
        let (_, mut actor, _) = create_actor();

        // Register a parent session
        let session = create_test_session("parent-session");
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(session),
            respond_to: tx,
        });

        // Send SubagentStart
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
            session_id: SessionId::new("parent-session"),
            event: LifecycleEvent::ChildSessionStart {
                id: Some("agent-xyz-456".into()),
                role: Some("plan".into()),
            },
            harness: atm_core::Harness::Unknown,
            pid: None,
            tmux_pane: None,
            context: LifecycleContext::default(),
            respond_to: tx,
        });
        assert_eq!(actor.pending_subagent_count(), 1);

        // Send SubagentStop with same agent_id
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
            session_id: SessionId::new("parent-session"),
            event: LifecycleEvent::ChildSessionEnd {
                id: Some("agent-xyz-456".into()),
            },
            harness: atm_core::Harness::Unknown,
            pid: None,
            tmux_pane: None,
            context: LifecycleContext::default(),
            respond_to: tx,
        });

        let result = rx.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(actor.pending_subagent_count(), 0);
    }

    #[tokio::test]
    async fn test_pending_subagent_ttl_cleanup() {
        let (_, mut actor, _) = create_actor();

        // Register a parent session
        let session = create_test_session("parent-session");
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(session),
            respond_to: tx,
        });

        // Send SubagentStart to create a pending entry
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
            session_id: SessionId::new("parent-session"),
            event: LifecycleEvent::ChildSessionStart {
                id: Some("agent-expired".into()),
                role: Some("explore".into()),
            },
            harness: atm_core::Harness::Unknown,
            pid: None,
            tmux_pane: None,
            context: LifecycleContext::default(),
            respond_to: tx,
        });
        assert_eq!(actor.pending_subagent_count(), 1);

        // Manually expire the pending entry by replacing created_at with a past instant
        // The TTL is 30 seconds, so we need to go back at least 31 seconds
        if let Some((_, pending)) = actor
            .pending_subagents
            .iter_mut()
            .find(|(id, _)| id == "agent-expired")
        {
            pending.created_at = Instant::now() - Duration::from_secs(31);
        }

        // Trigger cleanup (which also cleans pending subagents)
        actor.handle_command(RegistryCommand::CleanupStale);

        // Pending entry should be removed by TTL cleanup
        assert_eq!(actor.pending_subagent_count(), 0);
    }

    #[tokio::test]
    async fn test_subagent_correlation_links_parent_child() {
        let (_, mut actor, _) = create_actor();

        let parent_pid = std::process::id();
        let parent_id = SessionId::new("parent-session");

        // Register parent session via discovery
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::RegisterDiscovered {
            session_id: parent_id.clone(),
            pid: parent_pid,
            cwd: std::path::PathBuf::from("/home/user/project"),
            tmux_pane: None,
            harness: atm_core::Harness::Unknown,
            respond_to: tx,
        });

        // Send SubagentStart to create a pending correlation entry
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
            session_id: parent_id.clone(),
            event: LifecycleEvent::ChildSessionStart {
                id: Some("sub-agent-001".into()),
                role: Some("explore".into()),
            },
            harness: atm_core::Harness::Unknown,
            pid: Some(parent_pid),
            tmux_pane: None,
            context: LifecycleContext::default(),
            respond_to: tx,
        });
        assert_eq!(actor.pending_subagent_count(), 1);

        // Spawn a real child process so we have a descendant PID
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("failed to spawn sleep process");
        let child_pid = child.id();

        // Register the child session via discovery — this triggers try_correlate_subagent
        let child_id = SessionId::new("child-session");
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::RegisterDiscovered {
            session_id: child_id.clone(),
            pid: child_pid,
            cwd: std::path::PathBuf::from("/home/user/project"),
            tmux_pane: None,
            harness: atm_core::Harness::Unknown,
            respond_to: tx,
        });

        // The pending subagent should be consumed by correlation
        // because child_pid is a descendant of parent_pid (our process)
        assert_eq!(
            actor.pending_subagent_count(),
            0,
            "Pending subagent should be consumed by correlation"
        );

        // Verify parent → child link
        if let Some((parent_session, _)) = actor.sessions_by_pid.get(&parent_pid) {
            assert!(
                parent_session.child_session_ids.contains(&child_id),
                "Parent should list child in child_session_ids"
            );
        } else {
            panic!("Parent session not found");
        }

        // Verify child → parent link
        if let Some((child_session, _)) = actor.sessions_by_pid.get(&child_pid) {
            assert_eq!(
                child_session.parent_session_id.as_ref(),
                Some(&parent_id),
                "Child should reference parent_session_id"
            );
        } else {
            panic!("Child session not found");
        }

        // Clean up the sleep process
        let _ = child.kill();
        let _ = child.wait();
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
            harness: atm_core::Harness::Unknown,
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

    // ========================================================================
    // CWD Change Detection Tests
    // ========================================================================

    #[tokio::test]
    async fn test_refresh_git_info_detects_branch_change() {
        let (_cmd_tx, mut actor, mut event_rx) = create_actor();

        // Create a temp repo
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("refresh-repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        // Register a discovered session
        let current_pid = std::process::id();
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::RegisterDiscovered {
            session_id: SessionId::new("refresh-test"),
            pid: current_pid,
            cwd: repo.clone(),
            tmux_pane: None,
            harness: atm_core::Harness::Unknown,
            respond_to: tx,
        });
        rx.await.unwrap().unwrap();

        // Drain events
        while event_rx.try_recv().is_ok() {}

        // Verify initial branch
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: SessionId::new("refresh-test"),
            respond_to: tx,
        });
        let view = rx.await.unwrap().unwrap();
        assert_eq!(view.worktree_branch.as_deref(), Some("main"));

        // Change branch on disk
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/develop\n").unwrap();

        // Trigger git info refresh
        actor.handle_command(RegistryCommand::RefreshGitInfo);

        // Verify branch was updated
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: SessionId::new("refresh-test"),
            respond_to: tx,
        });
        let view = rx.await.unwrap().unwrap();
        assert_eq!(
            view.worktree_branch.as_deref(),
            Some("develop"),
            "branch should be updated after RefreshGitInfo"
        );

        // Should have published an Updated event
        let event = event_rx.try_recv();
        assert!(
            matches!(event, Ok(SessionEvent::Updated { .. })),
            "should publish Updated event on branch change"
        );
    }

    #[tokio::test]
    async fn test_refresh_git_info_no_change_no_event() {
        let (_cmd_tx, mut actor, mut event_rx) = create_actor();

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("no-change-repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        let current_pid = std::process::id();
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::RegisterDiscovered {
            session_id: SessionId::new("no-change-test"),
            pid: current_pid,
            cwd: repo.clone(),
            tmux_pane: None,
            harness: atm_core::Harness::Unknown,
            respond_to: tx,
        });
        rx.await.unwrap().unwrap();

        // Drain events from registration
        while event_rx.try_recv().is_ok() {}

        // Trigger refresh without changing anything
        actor.handle_command(RegistryCommand::RefreshGitInfo);

        // Should NOT publish any event
        let event = event_rx.try_recv();
        assert!(
            event.is_err(),
            "should NOT publish event when nothing changed"
        );
    }

    #[tokio::test]
    async fn test_rediscovery_preserves_domain_metadata() {
        let (_cmd_tx, mut actor, _event_rx) = create_actor();

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("preserve-repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        let current_pid = std::process::id();

        // Register initial discovery
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::RegisterDiscovered {
            session_id: SessionId::new("pending-1"),
            pid: current_pid,
            cwd: repo.clone(),
            tmux_pane: None,
            harness: atm_core::Harness::Unknown,
            respond_to: tx,
        });
        rx.await.unwrap().unwrap();

        // Upgrade via status line (accumulate cost)
        let status_json = serde_json::json!({
            "session_id": "real-id",
            "pid": current_pid,
            "cwd": repo.to_str().unwrap(),
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 2.50, "total_duration_ms": 120000},
            "context_window": {
                "total_input_tokens": 80000,
                "total_output_tokens": 20000,
                "context_window_size": 200000
            }
        });
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::UpdateFromStatusLine {
            session_id: SessionId::new("real-id"),
            data: status_json,
            respond_to: tx,
        });
        rx.await.unwrap().unwrap();

        // Verify cost accumulated
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: SessionId::new("real-id"),
            respond_to: tx,
        });
        let view = rx.await.unwrap().unwrap();
        assert!(view.cost_usd > 2.0, "cost should be ~2.50");

        // Re-discover (simulating rescan)
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::RegisterDiscovered {
            session_id: SessionId::new("pending-rescan"),
            pid: current_pid,
            cwd: repo.clone(),
            tmux_pane: None,
            harness: atm_core::Harness::Unknown,
            respond_to: tx,
        });
        rx.await.unwrap().unwrap();

        // Verify metadata preserved under new session_id
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: SessionId::new("pending-rescan"),
            respond_to: tx,
        });
        let view = rx.await.unwrap().unwrap();
        assert!(
            view.cost_usd > 2.0,
            "cost should be preserved after rescan, got {}",
            view.cost_usd
        );

        // Old session_id should no longer exist
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: SessionId::new("real-id"),
            respond_to: tx,
        });
        let old = rx.await.unwrap();
        assert!(old.is_none(), "old session_id should be removed from index");
    }

    #[tokio::test]
    async fn test_update_from_status_line_cwd_change_re_resolves_git() {
        let (_cmd_tx, mut actor, _event_rx) = create_actor();

        let dir = tempfile::tempdir().unwrap();
        let repo_a = dir.path().join("repo-a");
        let repo_b = dir.path().join("repo-b");
        std::fs::create_dir_all(repo_a.join(".git")).unwrap();
        std::fs::create_dir_all(repo_b.join(".git")).unwrap();
        std::fs::write(repo_a.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(repo_b.join(".git/HEAD"), "ref: refs/heads/feature\n").unwrap();

        let current_pid = std::process::id();

        // Register in repo_a
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::RegisterDiscovered {
            session_id: SessionId::new("cwd-test"),
            pid: current_pid,
            cwd: repo_a.clone(),
            tmux_pane: None,
            harness: atm_core::Harness::Unknown,
            respond_to: tx,
        });
        rx.await.unwrap().unwrap();

        // Verify initial state
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: SessionId::new("cwd-test"),
            respond_to: tx,
        });
        let view = rx.await.unwrap().unwrap();
        assert_eq!(view.worktree_branch.as_deref(), Some("main"));

        // Send status line with cwd changed to repo_b
        let status_json = serde_json::json!({
            "session_id": "cwd-test",
            "pid": current_pid,
            "cwd": repo_b.to_str().unwrap(),
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.50, "total_duration_ms": 5000},
            "context_window": {"total_input_tokens": 1000, "context_window_size": 200000}
        });
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::UpdateFromStatusLine {
            session_id: SessionId::new("cwd-test"),
            data: status_json,
            respond_to: tx,
        });
        rx.await.unwrap().unwrap();

        // Verify git info re-resolved
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::GetSession {
            session_id: SessionId::new("cwd-test"),
            respond_to: tx,
        });
        let view = rx.await.unwrap().unwrap();
        assert_eq!(
            view.worktree_branch.as_deref(),
            Some("feature"),
            "branch should be re-resolved after cwd change"
        );
        assert_eq!(
            view.project_root.as_deref(),
            Some(repo_b.to_str().unwrap()),
            "project_root should point to repo_b"
        );
    }

    fn register_test_session(actor: &mut RegistryActor, id: &str) -> SessionId {
        let session_id = SessionId::new(id);
        let (respond_to, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(create_test_session(id)),
            respond_to,
        });
        session_id
    }

    fn apply_with_context(
        actor: &mut RegistryActor,
        parent: &SessionId,
        event: LifecycleEvent,
        context: LifecycleContext,
    ) {
        let (respond_to, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
            session_id: parent.clone(),
            event,
            harness: atm_core::Harness::ClaudeCode,
            pid: None,
            tmux_pane: None,
            context,
            respond_to,
        });
    }

    fn session_view(actor: &RegistryActor, id: &str) -> Option<SessionView> {
        actor.handle_get_session(&SessionId::new(id))
    }

    fn start_child(actor: &mut RegistryActor, parent: &SessionId, id: &str) {
        apply_with_context(
            actor,
            parent,
            LifecycleEvent::ChildSessionStart {
                id: Some(id.into()),
                role: Some("general-purpose".into()),
            },
            LifecycleContext::default(),
        );
    }

    #[test]
    fn in_process_child_lifecycle_and_event_routing() {
        let (_, mut actor, _) = create_actor();
        let parent = register_test_session(&mut actor, "lead");
        if let Some((session, _)) = actor.sessions_by_pid.values_mut().next() {
            session.tmux_pane = Some("%7".into());
            session.project_root = Some("/repo".into());
        }

        start_child(&mut actor, &parent, "agent-1");
        let child = session_view(&actor, "agent-1").expect("child created");
        assert_eq!(child.parent_session_id, Some(parent.clone()));
        assert_eq!(child.tmux_pane.as_deref(), Some("%7"));
        assert_eq!(child.project_root.as_deref(), Some("/repo"));

        apply_with_context(
            &mut actor,
            &parent,
            LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::PermissionGate { tool: Tool::Bash },
            },
            LifecycleContext {
                child_id: Some("agent-1".into()),
                child_role: Some("general-purpose".into()),
                ..LifecycleContext::default()
            },
        );
        assert_eq!(
            session_view(&actor, "agent-1").map(|view| view.status),
            Some(atm_core::SessionStatus::AttentionNeeded)
        );
        assert_eq!(
            session_view(&actor, "lead").map(|view| view.status),
            Some(atm_core::SessionStatus::Working)
        );

        apply_with_context(
            &mut actor,
            &parent,
            LifecycleEvent::ChildSessionEnd {
                id: Some("agent-1".into()),
            },
            LifecycleContext::default(),
        );
        assert!(session_view(&actor, "agent-1").is_none());
        assert!(session_view(&actor, "lead").is_some_and(|view| view.child_session_ids.is_empty()));
    }

    #[test]
    fn teammate_alias_handles_both_arrival_orders_and_is_parent_scoped() {
        let (_, mut actor, _) = create_actor();
        let first = register_test_session(&mut actor, "lead-a");
        let second = register_test_session(&mut actor, "lead-b");

        apply_with_context(
            &mut actor,
            &first,
            LifecycleEvent::WorkingStart,
            LifecycleContext {
                child_alias: Some(("reviewer".into(), "agent-a".into())),
                ..LifecycleContext::default()
            },
        );
        start_child(&mut actor, &first, "agent-a");

        apply_with_context(
            &mut actor,
            &second,
            LifecycleEvent::Idle,
            LifecycleContext {
                child_name: Some("reviewer".into()),
                child_role: Some("teammate".into()),
                ..LifecycleContext::default()
            },
        );
        assert!(session_view(&actor, "reviewer@lead-b").is_some());
        apply_with_context(
            &mut actor,
            &second,
            LifecycleEvent::WorkingStart,
            LifecycleContext {
                child_alias: Some(("reviewer".into(), "agent-b".into())),
                ..LifecycleContext::default()
            },
        );
        start_child(&mut actor, &second, "agent-b");

        for (parent, expected) in [(&first, "agent-a"), (&second, "reviewer@lead-b")] {
            apply_with_context(
                &mut actor,
                parent,
                LifecycleEvent::NeedsInput {
                    reason: NeedsInputReason::PermissionGate { tool: Tool::Bash },
                },
                LifecycleContext {
                    child_name: Some("reviewer".into()),
                    ..LifecycleContext::default()
                },
            );
            assert_eq!(
                session_view(&actor, expected).map(|view| view.status),
                Some(atm_core::SessionStatus::AttentionNeeded)
            );
        }
        assert_eq!(actor.session_count(), 4);
    }

    #[test]
    fn parent_cleanup_sweeps_only_synthetic_children() {
        let (_, mut actor, _) = create_actor();
        let parent = register_test_session(&mut actor, "lead");
        start_child(&mut actor, &parent, "synthetic");

        let mut process_child = create_test_session("process-child");
        process_child.parent_session_id = Some(parent.clone());
        let pid = std::process::id();
        let mut infra = SessionInfrastructure::new();
        infra.set_pid(pid);
        actor.sessions_by_pid.insert(pid, (process_child, infra));
        actor
            .session_id_to_pid
            .insert(SessionId::new("process-child"), pid);
        if let Some(parent_pid) = actor.session_id_to_pid.get(&parent).copied() {
            if let Some((session, _)) = actor.sessions_by_pid.get_mut(&parent_pid) {
                session
                    .child_session_ids
                    .push(SessionId::new("process-child"));
            }
        }

        let (respond_to, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Remove {
            session_id: parent,
            respond_to,
        });
        assert!(session_view(&actor, "synthetic").is_none());
        assert!(session_view(&actor, "process-child")
            .is_some_and(|view| view.parent_session_id.is_none()));
    }

    #[test]
    fn quiet_stop_sweeps_children_but_busy_stop_keeps_them() {
        let (_, mut actor, _) = create_actor();
        let parent = register_test_session(&mut actor, "lead");
        start_child(&mut actor, &parent, "agent-1");
        apply_with_context(
            &mut actor,
            &parent,
            LifecycleEvent::WorkingEnd,
            LifecycleContext {
                background_activity: Some((2, 1)),
                ..LifecycleContext::default()
            },
        );
        assert!(session_view(&actor, "agent-1").is_some());
        assert_eq!(
            session_view(&actor, "lead").and_then(|view| view.activity_detail),
            Some("2 bg tasks, 1 scheduled".into())
        );
        apply_with_context(
            &mut actor,
            &parent,
            LifecycleEvent::WorkingEnd,
            LifecycleContext {
                background_activity: Some((0, 0)),
                ..LifecycleContext::default()
            },
        );
        assert!(session_view(&actor, "agent-1").is_none());
    }
}
