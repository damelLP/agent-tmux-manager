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
    AgentType, ChildAgentRef, LifecycleEvent, Model, NeedsInputReason, SessionDomain, SessionId,
    SessionInfrastructure, SessionView,
};
use atm_protocol::RawStatusLine;

use super::commands::{RegistryCommand, RegistryError, RemovalReason, SessionEvent};

// ============================================================================
// Resource Limits (from RESOURCE_LIMITS.md)
// ============================================================================

/// Maximum number of sessions the registry can hold.
pub const MAX_SESSIONS: usize = 100;

/// Keys at or above this value are synthetic: they belong to sessions
/// without a process of their own (in-process child agents, test
/// fixtures) and never collide with real PIDs.
const SYNTHETIC_PID_BASE: u32 = 0x8000_0000;

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

    /// In-process child agents (Claude subagents, in-process teammates)
    /// keyed by vendor `agent_id`. They share the parent's process, so
    /// they live under synthetic keys in `sessions_by_pid`; this index
    /// routes their tagged events and removes them when they finish.
    children_by_agent_id: HashMap<String, u32>,

    /// `(parent session, name)` → child key, for in-process children
    /// known by the human name they were spawned with (Agent tool
    /// `name`). Teammate events name the teammate instead of carrying
    /// its agent id, and names are only unique within a session, hence
    /// the parent in the key. The value is a vendor agent id, or a
    /// name-placeholder key when the child was seen by name first.
    child_aliases: HashMap<(SessionId, String), String>,
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
            children_by_agent_id: HashMap::new(),
            child_aliases: HashMap::new(),
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
                child_agent,
                respond_to,
            } => {
                let result = self.handle_apply_lifecycle_event(
                    session_id,
                    event,
                    harness,
                    pid,
                    tmux_pane,
                    child_agent,
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
            RegistryCommand::RegisterChildAlias {
                parent,
                name,
                agent_id,
            } => {
                self.handle_register_child_alias(&parent, &name, &agent_id);
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

    /// Generates a synthetic PID for sessions without a process of their
    /// own: in-process child agents, and test fixtures registered
    /// without a PID.
    fn generate_synthetic_pid(&self) -> u32 {
        // Use high PID range unlikely to conflict with real processes
        let base = SYNTHETIC_PID_BASE;
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
        let child_ids = session.child_session_ids.clone();

        self.session_id_to_pid.remove(&old_id);
        self.session_id_to_pid.insert(new_id.clone(), pid);

        // In-process children point back at the parent by id; keep
        // them attached across the rename.
        for child_id in &child_ids {
            if let Some((child, _)) = self
                .session_id_to_pid
                .get(child_id)
                .copied()
                .and_then(|cp| self.sessions_by_pid.get_mut(&cp))
            {
                child.parent_session_id = Some(new_id.clone());
            }
        }

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
        child_agent: Option<ChildAgentRef>,
    ) -> Result<(), RegistryError> {
        // SessionEnd: remove session immediately.
        if matches!(event, LifecycleEvent::SessionEnd { .. }) {
            let target_pid = pid.or_else(|| self.session_id_to_pid.get(&session_id).copied());

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

        // Find session by PID first (preferred), then by session_id
        let mut target_pid = pid.or_else(|| self.session_id_to_pid.get(&session_id).copied());

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

        // Session doesn't exist yet - this is normal due to race
        // conditions (and to a daemon restarted mid-run, whose first
        // sight of a process may be a child's tool event). With PID as
        // primary key, we can create the session now if we have a PID.
        if target_pid.is_none_or(|p| !self.sessions_by_pid.contains_key(&p)) {
            let Some(p) = pid.filter(|p| *p != 0) else {
                debug!(
                    session_id = %session_id,
                    event = ?event,
                    "Lifecycle event for non-existent session without PID, ignoring"
                );
                return Ok(());
            };
            debug!(
                session_id = %session_id,
                pid = p,
                event = ?event,
                "Creating session from lifecycle event"
            );
            self.create_session_for_pid(&session_id, p, harness, tmux_pane.clone());
            target_pid = Some(p);
        }

        // Child bookkeeping. `ChildSessionStart` creates the child
        // session immediately: Claude runs subagents and in-process
        // teammates inside the parent's process, so no new PID ever
        // appears to correlate against. The pending list is kept for
        // children that *are* separate processes; if one later
        // registers and proves PID ancestry it supersedes the
        // in-process placeholder (see `try_correlate_subagent`).
        match &event {
            LifecycleEvent::ChildSessionStart {
                id: Some(aid),
                role,
            } => {
                if let Some(parent_pid) = target_pid {
                    self.record_pending_subagent(parent_pid, &session_id, aid, role.as_deref());
                    self.ensure_child_session(parent_pid, aid, role.as_deref(), harness);
                }
            }
            LifecycleEvent::ChildSessionEnd {
                id: Some(aid),
                reason,
                last_message,
            } => {
                self.pending_subagents.retain(|(id, _)| id != aid);
                self.finish_child_session(aid, reason.as_deref(), last_message.as_deref());
            }
            _ => {}
        }

        // Events tagged with the child agent they were fired from (or
        // are about) are the child's own activity and must not repaint
        // the parent. `ChildSessionStart` / `End` are excluded upstream
        // because they are *about* the child.
        if let Some(child) = child_agent.as_ref() {
            if let Some(parent_pid) = target_pid {
                self.route_child_event(parent_pid, child, &event, tool_name.as_deref(), harness);
            }
            // A child's event must never repaint the parent.
            return Ok(());
        }

        let Some(p) = target_pid else {
            return Ok(());
        };
        self.apply_to_session(p, &event, tool_name.as_deref(), tmux_pane);

        // The turn ended with nothing left running: any child still
        // marked working missed its `SubagentStop` and can be swept.
        if let LifecycleEvent::BackgroundActivity {
            running_tasks: 0, ..
        } = event
        {
            self.sweep_working_children(p);
        }

        Ok(())
    }

    /// Applies a lifecycle event to the session stored at `pid`,
    /// records tool usage, and publishes the updated view. No-op when
    /// the key is unknown.
    fn apply_to_session(
        &mut self,
        pid: u32,
        event: &LifecycleEvent,
        tool_name: Option<&str>,
        tmux_pane: Option<String>,
    ) {
        let Some((session, infra)) = self.sessions_by_pid.get_mut(&pid) else {
            return;
        };

        session.apply_lifecycle_event(event);
        session.set_first_prompt_from_event(event);

        if tmux_pane.is_some() && session.tmux_pane.is_none() {
            session.tmux_pane = tmux_pane;
        }

        debug!(
            session_id = %session.id,
            event = ?event,
            new_status = %session.status,
            "Lifecycle event applied"
        );

        if let Some(name) = tool_name {
            infra.record_tool_use(name, None);
        }

        let view = SessionView::from_domain(session);
        let _ = self.event_publisher.send(SessionEvent::Updated {
            session: Box::new(view),
        });
    }

    /// Creates a session for a process we have not seen before, seeded
    /// from `/proc/{pid}/cwd` so it lands under the right project and
    /// branch from frame one (otherwise it falls into the "Other" tree
    /// bucket). Publishes `Registered` and runs subagent correlation.
    fn create_session_for_pid(
        &mut self,
        session_id: &SessionId,
        pid: u32,
        harness: atm_core::Harness,
        tmux_pane: Option<String>,
    ) {
        let proc_cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
        let session = build_session_from_pid(
            session_id.clone(),
            AgentType::GeneralPurpose,
            Model::Unknown,
            harness,
            tmux_pane,
            proc_cwd,
        );
        let agent_type = session.agent_type.clone();

        let mut infra = SessionInfrastructure::new();
        infra.set_pid(pid);

        self.sessions_by_pid.insert(pid, (session, infra));
        self.session_id_to_pid.insert(session_id.clone(), pid);

        let _ = self.event_publisher.send(SessionEvent::Registered {
            session_id: session_id.clone(),
            agent_type,
        });

        self.try_correlate_subagent(session_id, pid);
    }

    /// Records a `SubagentStart` for later PID-ancestry correlation, in
    /// case the child turns out to be a separate process.
    fn record_pending_subagent(
        &mut self,
        parent_pid: u32,
        fallback_session_id: &SessionId,
        agent_id: &str,
        role: Option<&str>,
    ) {
        let parent_session_id = self
            .sessions_by_pid
            .get(&parent_pid)
            .map(|(s, _)| s.id.clone())
            .unwrap_or_else(|| fallback_session_id.clone());
        let parent_start_time = crate::tmux::get_process_start_time(parent_pid);
        let agent_type = child_agent_type(role);

        self.pending_subagents.push((
            agent_id.to_string(),
            PendingSubagent {
                parent_session_id,
                parent_pid,
                parent_start_time,
                agent_type,
                created_at: Instant::now(),
            },
        ));
    }

    /// Creates (or finds) the session for an in-process child agent of
    /// the parent stored at `parent_pid`, returning its synthetic key.
    ///
    /// The child inherits the parent's pane, project, and model so it
    /// nests under the parent in the tree and jumps to the same pane.
    /// It starts `Working` because a subagent runs the moment it is
    /// spawned. Returns `None` if the parent is unknown or the registry
    /// is full.
    fn ensure_child_session(
        &mut self,
        parent_pid: u32,
        agent_id: &str,
        role: Option<&str>,
        harness: atm_core::Harness,
    ) -> Option<u32> {
        if let Some(existing) = self.children_by_agent_id.get(agent_id) {
            return Some(*existing);
        }
        if self.sessions_by_pid.len() >= MAX_SESSIONS {
            warn!(
                agent_id,
                max = MAX_SESSIONS,
                "Registry full, cannot register child agent session"
            );
            return None;
        }

        let child_id = SessionId::new(agent_id);
        let (parent_id, mut child) = {
            let (parent, _) = self.sessions_by_pid.get(&parent_pid)?;
            let agent_type = child_agent_type(role);
            let mut child = SessionDomain::new(child_id.clone(), agent_type, parent.model);
            child.harness = harness;
            child.model_display_override = parent.model_display_override.clone();
            child.tmux_pane = parent.tmux_pane.clone();
            child.working_directory = parent.working_directory.clone();
            child.project_root = parent.project_root.clone();
            child.worktree_path = parent.worktree_path.clone();
            child.worktree_branch = parent.worktree_branch.clone();
            child.parent_session_id = Some(parent.id.clone());
            (parent.id.clone(), child)
        };
        child.apply_lifecycle_event(&LifecycleEvent::WorkingStart);

        let child_pid = self.generate_synthetic_pid();
        let agent_type = child.agent_type.clone();
        let child_view = SessionView::from_domain(&child);

        self.sessions_by_pid
            .insert(child_pid, (child, SessionInfrastructure::new()));
        self.session_id_to_pid.insert(child_id.clone(), child_pid);
        self.children_by_agent_id
            .insert(agent_id.to_string(), child_pid);

        let parent_view = self
            .sessions_by_pid
            .get_mut(&parent_pid)
            .map(|(parent, _)| {
                if !parent.child_session_ids.contains(&child_id) {
                    parent.child_session_ids.push(child_id.clone());
                }
                SessionView::from_domain(parent)
            });

        info!(
            child_session_id = %child_id,
            parent_session_id = %parent_id,
            role = ?role,
            "Child agent session registered"
        );

        let _ = self.event_publisher.send(SessionEvent::Registered {
            session_id: child_id,
            agent_type,
        });
        let _ = self.event_publisher.send(SessionEvent::Updated {
            session: Box::new(child_view),
        });
        if let Some(view) = parent_view {
            let _ = self.event_publisher.send(SessionEvent::Updated {
                session: Box::new(view),
            });
        }

        Some(child_pid)
    }

    /// Applies an event tagged with a child reference to that child's
    /// session, creating the session if this is the first sight of it.
    ///
    /// Resolution order: the vendor agent id when the event carries one
    /// (recording the name → id pairing if it carries both), otherwise
    /// the name through this parent's aliases, otherwise a placeholder
    /// keyed by name and parent. A child owned by another parent is
    /// never touched: names repeat across sessions, and a vendor id
    /// must not be repainted from a different session either.
    fn route_child_event(
        &mut self,
        parent_pid: u32,
        child: &ChildAgentRef,
        event: &LifecycleEvent,
        tool_name: Option<&str>,
        harness: atm_core::Harness,
    ) {
        let Some(parent_id) = self
            .sessions_by_pid
            .get(&parent_pid)
            .map(|(parent, _)| parent.id.clone())
        else {
            return;
        };

        let key = match (&child.id, &child.name) {
            (Some(id), name) => {
                if let Some(name) = name {
                    self.handle_register_child_alias(&parent_id, name, id);
                }
                id.clone()
            }
            (None, Some(name)) => self
                .child_aliases
                .get(&(parent_id.clone(), name.clone()))
                .cloned()
                .unwrap_or_else(|| name_placeholder_key(&parent_id, name)),
            (None, None) => return,
        };

        let child_pid = self.children_by_agent_id.get(&key).copied().or_else(|| {
            // Missed `SubagentStart` (daemon restart, dropped hook,
            // or a name-only event before the spawn reported its
            // id): materialize the child from this event.
            let pid =
                self.ensure_child_session(parent_pid, &key, child.role.as_deref(), harness)?;
            if let Some(name) = &child.name {
                self.child_aliases
                    .insert((parent_id.clone(), name.clone()), key.clone());
            }
            Some(pid)
        });

        match child_pid {
            Some(cp) => {
                let owned = self
                    .sessions_by_pid
                    .get(&cp)
                    .is_some_and(|(s, _)| s.parent_session_id.as_ref() == Some(&parent_id));
                if owned {
                    self.apply_to_session(cp, event, tool_name, None);
                } else {
                    warn!(
                        child = %key,
                        session_id = %parent_id,
                        "Dropping child agent event: the child belongs to another session"
                    );
                }
            }
            None => warn!(
                child = %key,
                session_id = %parent_id,
                event = ?event,
                "Dropping child agent event: no session could be created for it"
            ),
        }
    }

    /// Records `(parent, name)` → `agent_id`. Kept even before the child
    /// registers: hooks arrive over separate connections, so the
    /// spawning call's response can land before `SubagentStart`. If the
    /// child was first seen by name (a placeholder), it is re-keyed to
    /// the agent id; if both a placeholder and the id-keyed child exist,
    /// the placeholder is folded away.
    fn handle_register_child_alias(&mut self, parent: &SessionId, name: &str, agent_id: &str) {
        let name = name.trim();
        let agent_id = agent_id.trim();
        if name.is_empty() || agent_id.is_empty() {
            return;
        }
        let alias_key = (parent.clone(), name.to_string());
        if self
            .child_aliases
            .get(&alias_key)
            .is_some_and(|known| known == agent_id)
        {
            return;
        }

        let placeholder = name_placeholder_key(parent, name);
        if let Some(pid) = self.children_by_agent_id.remove(&placeholder) {
            if self.children_by_agent_id.contains_key(agent_id) {
                if let Some((stale, _)) = self.sessions_by_pid.remove(&pid) {
                    self.session_id_to_pid.remove(&stale.id);
                    self.unlink_child_from_parent(&stale);
                    let _ = self.event_publisher.send(SessionEvent::Removed {
                        session_id: stale.id,
                        reason: RemovalReason::Upgraded,
                    });
                }
            } else {
                self.children_by_agent_id.insert(agent_id.to_string(), pid);
            }
        }

        debug!(parent = %parent, name, agent_id, "Child agent alias recorded");
        self.child_aliases.insert(alias_key, agent_id.to_string());
    }

    /// Drops index entries for children no longer in the registry, and
    /// aliases whose parent session is gone. Aliases for children that
    /// have not registered yet are kept on purpose.
    fn prune_child_indexes(&mut self) {
        let sessions = &self.sessions_by_pid;
        self.children_by_agent_id
            .retain(|_, pid| sessions.contains_key(pid));
        let parents = &self.session_id_to_pid;
        self.child_aliases
            .retain(|(parent, _), _| parents.contains_key(parent));
    }

    /// Removes the in-process child for `agent_id` after the vendor
    /// reported it finished, logging how it ended.
    fn finish_child_session(
        &mut self,
        agent_id: &str,
        reason: Option<&str>,
        last_message: Option<&str>,
    ) {
        let Some(child_pid) = self.children_by_agent_id.remove(agent_id) else {
            return;
        };
        self.child_aliases.retain(|_, id| id != agent_id);
        let Some((child, _)) = self.sessions_by_pid.remove(&child_pid) else {
            return;
        };
        self.session_id_to_pid.remove(&child.id);

        info!(
            child_session_id = %child.id,
            parent_session_id = ?child.parent_session_id,
            reason = ?reason,
            summary = %summarize(last_message),
            "Child agent session finished"
        );

        self.unlink_child_from_parent(&child);
        let _ = self.event_publisher.send(SessionEvent::Removed {
            session_id: child.id,
            reason: RemovalReason::SessionEnded,
        });
    }

    /// Drops `child` from its parent's `child_session_ids` and publishes
    /// the parent's updated view.
    fn unlink_child_from_parent(&mut self, child: &SessionDomain) {
        let Some(parent_pid) = child
            .parent_session_id
            .as_ref()
            .and_then(|id| self.session_id_to_pid.get(id))
            .copied()
        else {
            return;
        };
        if let Some((parent, _)) = self.sessions_by_pid.get_mut(&parent_pid) {
            parent.child_session_ids.retain(|id| id != &child.id);
            let view = SessionView::from_domain(parent);
            let _ = self.event_publisher.send(SessionEvent::Updated {
                session: Box::new(view),
            });
        }
    }

    /// Tears down the links of a session that has just been removed from
    /// the primary map. In-process children have no process of their own
    /// and would otherwise linger as "alive" forever, so they go with the
    /// parent. Process-backed children (correlated by PID ancestry)
    /// outlive it and are surfaced at top level instead. A removed child
    /// is detached from its parent.
    fn detach_removed_session(&mut self, session: &SessionDomain, reason: RemovalReason) {
        for child_id in &session.child_session_ids {
            let Some(child_pid) = self.session_id_to_pid.get(child_id).copied() else {
                continue;
            };
            if child_pid < SYNTHETIC_PID_BASE {
                if let Some((child, _)) = self.sessions_by_pid.get_mut(&child_pid) {
                    child.parent_session_id = None;
                    let view = SessionView::from_domain(child);
                    let _ = self.event_publisher.send(SessionEvent::Updated {
                        session: Box::new(view),
                    });
                }
                continue;
            }
            self.session_id_to_pid.remove(child_id);
            self.sessions_by_pid.remove(&child_pid);
            info!(
                child_session_id = %child_id,
                parent_session_id = %session.id,
                reason = %reason,
                "Child agent session removed with parent"
            );
            let _ = self.event_publisher.send(SessionEvent::Removed {
                session_id: child_id.clone(),
                reason,
            });
        }
        self.prune_child_indexes();
        if session.parent_session_id.is_some() {
            self.unlink_child_from_parent(session);
        }
    }

    /// Removes children of the session at `parent_pid` that are still
    /// `Working` after the parent's turn ended with no background work,
    /// i.e. whose `SubagentStop` never arrived.
    fn sweep_working_children(&mut self, parent_pid: u32) {
        let Some(parent_id) = self
            .sessions_by_pid
            .get(&parent_pid)
            .map(|(parent, _)| parent.id.clone())
        else {
            return;
        };
        let stale: Vec<String> = self
            .children_by_agent_id
            .iter()
            .filter(|(_, child_pid)| {
                // Process-backed children run on their own; only
                // in-process ones can be orphaned by a missed stop.
                **child_pid >= SYNTHETIC_PID_BASE
                    && self
                        .sessions_by_pid
                        .get(*child_pid)
                        .is_some_and(|(child, _)| {
                            child.parent_session_id.as_ref() == Some(&parent_id)
                                && child.status == atm_core::SessionStatus::Working
                        })
            })
            .map(|(key, _)| key.clone())
            .collect();

        for key in stale {
            debug!(agent = %key, "Sweeping child left working after parent turn ended");
            self.finish_child_session(&key, Some("swept"), None);
        }
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

        let removed = self.sessions_by_pid.remove(&pid);
        if let Some((session, _)) = &removed {
            self.detach_removed_session(session, reason);
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

            // `ChildSessionStart` eagerly created an in-process
            // placeholder for this agent; the real process supersedes it.
            if let Some(placeholder_pid) = self.children_by_agent_id.remove(&agent_id) {
                if let Some((placeholder, _)) = self.sessions_by_pid.remove(&placeholder_pid) {
                    self.session_id_to_pid.remove(&placeholder.id);
                    if let Some((parent, _)) = self.sessions_by_pid.get_mut(&pending.parent_pid) {
                        parent.child_session_ids.retain(|id| id != &placeholder.id);
                    }
                    let _ = self.event_publisher.send(SessionEvent::Removed {
                        session_id: placeholder.id,
                        reason: RemovalReason::Upgraded,
                    });
                }
            }

            // Index the real child under its agent id too, so id- or
            // name-tagged events keep reaching it.
            self.children_by_agent_id.insert(agent_id.clone(), pid);

            // Link parent to child
            if let Some((parent_session, _)) = self.sessions_by_pid.get_mut(&pending.parent_pid) {
                parent_session.child_session_ids.push(session_id.clone());
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
            if let Some((session, _)) = &removed {
                self.detach_removed_session(session, RemovalReason::ProcessDied);
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

/// Key and session id for a child known only by name, unique per
/// parent so the same teammate name in two sessions cannot collide.
fn name_placeholder_key(parent: &SessionId, name: &str) -> String {
    format!("{name}@{}", parent.short())
}

/// Agent type for an in-process child. A subagent spawned as
/// `general-purpose` must not render as `main` (the label reserved for
/// the top-level agent), so it keeps its raw role instead.
fn child_agent_type(role: Option<&str>) -> AgentType {
    match role.map(str::trim).filter(|r| !r.is_empty()) {
        None => AgentType::Custom("subagent".to_string()),
        Some(r) => match AgentType::from_subagent_type(r) {
            AgentType::GeneralPurpose => AgentType::Custom(r.to_string()),
            other => other,
        },
    }
}

/// Longest excerpt of a child's final message kept in the daemon log.
const CHILD_SUMMARY_MAX_CHARS: usize = 120;

/// First line of `text`, truncated for logging; `-` when absent.
fn summarize(text: Option<&str>) -> String {
    let Some(text) = text else {
        return "-".to_string();
    };
    let first_line = text.lines().next().unwrap_or("").trim();
    let mut out: String = first_line.chars().take(CHILD_SUMMARY_MAX_CHARS).collect();
    if first_line.chars().count() > CHILD_SUMMARY_MAX_CHARS || text.lines().count() > 1 {
        out.push('…');
    }
    out
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
            child_agent: None,
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
            child_agent: None,
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
            child_agent: None,
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
            child_agent: None,
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
            child_agent: None,
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
            child_agent: None,
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
            child_agent: None,
            respond_to: tx,
        });
        assert_eq!(actor.pending_subagent_count(), 1);

        // Send SubagentStop with same agent_id
        let (tx, rx) = oneshot::channel();
        actor.handle_command(RegistryCommand::ApplyLifecycleEvent {
            session_id: SessionId::new("parent-session"),
            event: LifecycleEvent::ChildSessionEnd {
                id: Some("agent-xyz-456".into()),
                reason: None,
                last_message: None,
            },
            harness: atm_core::Harness::Unknown,
            pid: None,
            tmux_pane: None,
            child_agent: None,
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
            child_agent: None,
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
            child_agent: None,
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

    // ------------------------------------------------------------------
    // In-process child agents (Claude subagents / teammates)
    // ------------------------------------------------------------------

    fn lifecycle_cmd(
        session_id: &SessionId,
        event: LifecycleEvent,
        child_agent: Option<ChildAgentRef>,
    ) -> (
        RegistryCommand,
        oneshot::Receiver<Result<(), RegistryError>>,
    ) {
        let (tx, rx) = oneshot::channel();
        (
            RegistryCommand::ApplyLifecycleEvent {
                session_id: session_id.clone(),
                event,
                harness: atm_core::Harness::ClaudeCode,
                pid: None,
                tmux_pane: None,
                child_agent,
                respond_to: tx,
            },
            rx,
        )
    }

    fn by_id(id: &str, role: &str) -> Option<ChildAgentRef> {
        Some(ChildAgentRef {
            id: Some(id.into()),
            name: None,
            role: Some(role.into()),
        })
    }

    fn by_name(name: &str) -> Option<ChildAgentRef> {
        Some(ChildAgentRef {
            id: None,
            name: Some(name.into()),
            role: Some("teammate".into()),
        })
    }

    fn register_parent(actor: &mut RegistryActor, id: &str) -> SessionId {
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(create_test_session(id)),
            respond_to: tx,
        });
        SessionId::new(id)
    }

    fn alias(actor: &mut RegistryActor, parent: &SessionId, name: &str, agent_id: &str) {
        actor.handle_command(RegistryCommand::RegisterChildAlias {
            parent: parent.clone(),
            name: name.into(),
            agent_id: agent_id.into(),
        });
    }

    /// Registers a parent and sends `ChildSessionStart` for `agent_id`.
    async fn spawn_parent_with_child(
        actor: &mut RegistryActor,
        parent: &str,
        agent_id: &str,
    ) -> SessionId {
        let parent_id = SessionId::new(parent);
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::Register {
            session: Box::new(create_test_session(parent)),
            respond_to: tx,
        });
        let (cmd, rx) = lifecycle_cmd(
            &parent_id,
            LifecycleEvent::ChildSessionStart {
                id: Some(agent_id.into()),
                role: Some("explore".into()),
            },
            None,
        );
        actor.handle_command(cmd);
        assert!(rx.await.unwrap().is_ok());
        parent_id
    }

    fn view_of(actor: &RegistryActor, id: &str) -> Option<SessionView> {
        actor.handle_get_session(&SessionId::new(id))
    }

    #[tokio::test]
    async fn child_session_start_creates_in_process_child() {
        let (_, mut actor, _) = create_actor();
        let parent_id = spawn_parent_with_child(&mut actor, "parent", "agent-1").await;

        let child = view_of(&actor, "agent-1").expect("child session registered");
        assert_eq!(child.parent_session_id.as_ref(), Some(&parent_id));
        assert_eq!(child.status, atm_core::SessionStatus::Working);
        assert_eq!(child.agent_type, "explore");

        let parent = view_of(&actor, "parent").expect("parent still present");
        assert_eq!(parent.child_session_ids, vec![SessionId::new("agent-1")]);
        assert_eq!(actor.session_count(), 2);

        // A second SubagentStart for the same agent is idempotent.
        let (cmd, _) = lifecycle_cmd(
            &parent_id,
            LifecycleEvent::ChildSessionStart {
                id: Some("agent-1".into()),
                role: None,
            },
            None,
        );
        actor.handle_command(cmd);
        assert_eq!(actor.session_count(), 2);
        let parent = view_of(&actor, "parent").expect("parent");
        assert_eq!(parent.child_session_ids.len(), 1);
    }

    #[tokio::test]
    async fn events_tagged_with_child_agent_route_to_child() {
        let (_, mut actor, _) = create_actor();
        let parent_id = spawn_parent_with_child(&mut actor, "parent", "agent-1").await;

        // Parent goes idle; the child's permission wait must not
        // repaint it.
        let (cmd, _) = lifecycle_cmd(&parent_id, LifecycleEvent::WorkingEnd, None);
        actor.handle_command(cmd);
        let (cmd, _) = lifecycle_cmd(
            &parent_id,
            LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::PermissionGate { tool: Tool::Bash },
            },
            by_id("agent-1", "explore"),
        );
        actor.handle_command(cmd);

        let child = view_of(&actor, "agent-1").expect("child");
        assert_eq!(child.status, atm_core::SessionStatus::AttentionNeeded);
        let parent = view_of(&actor, "parent").expect("parent");
        assert_eq!(parent.status, atm_core::SessionStatus::Idle);
    }

    #[tokio::test]
    async fn child_event_without_start_materializes_child() {
        let (_, mut actor, _) = create_actor();
        let parent_id = register_parent(&mut actor, "parent");

        // Daemon missed SubagentStart: the child's first tool event
        // still creates a session for it.
        let (cmd, _) = lifecycle_cmd(
            &parent_id,
            LifecycleEvent::ToolCallStart {
                name: Tool::Read,
                tool_use_id: None,
                input: None,
            },
            by_id("late-agent", "code-reviewer"),
        );
        actor.handle_command(cmd);

        let child = view_of(&actor, "late-agent").expect("child materialized");
        assert_eq!(child.agent_type, "review");
        assert!(
            child
                .activity_detail
                .as_deref()
                .is_some_and(|a| a.contains("Read")),
            "child carries its own activity, got {:?}",
            child.activity_detail
        );
        assert_eq!(child.parent_session_id, Some(parent_id));
        let parent = view_of(&actor, "parent").expect("parent");
        assert_eq!(parent.child_session_ids, vec![SessionId::new("late-agent")]);
    }

    #[tokio::test]
    async fn child_session_end_removes_child_and_unlinks_parent() {
        let (_, mut actor, mut event_rx) = create_actor();
        let parent_id = spawn_parent_with_child(&mut actor, "parent", "agent-1").await;
        while event_rx.try_recv().is_ok() {}

        let (cmd, _) = lifecycle_cmd(
            &parent_id,
            LifecycleEvent::ChildSessionEnd {
                id: Some("agent-1".into()),
                reason: Some("end_turn".into()),
                last_message: Some("done".into()),
            },
            None,
        );
        actor.handle_command(cmd);

        assert!(view_of(&actor, "agent-1").is_none(), "child removed");
        let parent = view_of(&actor, "parent").expect("parent");
        assert!(parent.child_session_ids.is_empty());
        assert_eq!(actor.session_count(), 1);

        let mut removed = false;
        while let Ok(ev) = event_rx.try_recv() {
            if let SessionEvent::Removed { session_id, reason } = ev {
                if session_id.as_str() == "agent-1" {
                    assert_eq!(reason, RemovalReason::SessionEnded);
                    removed = true;
                }
            }
        }
        assert!(removed, "expected Removed event for child");
    }

    #[tokio::test]
    async fn removing_parent_cascades_to_children() {
        let (_, mut actor, _) = create_actor();
        let parent_id = spawn_parent_with_child(&mut actor, "parent", "agent-1").await;
        let (cmd, _) = lifecycle_cmd(
            &parent_id,
            LifecycleEvent::ChildSessionStart {
                id: Some("agent-2".into()),
                role: None,
            },
            None,
        );
        actor.handle_command(cmd);
        assert_eq!(actor.session_count(), 3);

        let (cmd, rx) = lifecycle_cmd(
            &parent_id,
            LifecycleEvent::SessionEnd { reason: None },
            None,
        );
        actor.handle_command(cmd);
        assert!(rx.await.unwrap().is_ok());
        assert_eq!(
            actor.session_count(),
            0,
            "children must not outlive their parent"
        );
    }

    #[tokio::test]
    async fn quiet_turn_end_sweeps_children_left_working() {
        let (_, mut actor, _) = create_actor();
        let parent_id = spawn_parent_with_child(&mut actor, "parent", "agent-1").await;
        // A teammate that reported idle must survive the sweep.
        let (cmd, _) = lifecycle_cmd(&parent_id, LifecycleEvent::Idle, by_id("mate-1", "worker"));
        actor.handle_command(cmd);
        assert_eq!(actor.session_count(), 3);

        for event in [
            LifecycleEvent::WorkingEnd,
            LifecycleEvent::BackgroundActivity {
                running_tasks: 0,
                scheduled_tasks: 0,
            },
        ] {
            let (cmd, _) = lifecycle_cmd(&parent_id, event, None);
            actor.handle_command(cmd);
        }

        assert!(
            view_of(&actor, "agent-1").is_none(),
            "working child without SubagentStop swept"
        );
        assert!(view_of(&actor, "mate-1").is_some(), "idle teammate kept");
        let parent = view_of(&actor, "parent").expect("parent");
        assert_eq!(parent.child_session_ids, vec![SessionId::new("mate-1")]);
    }

    #[tokio::test]
    async fn turn_end_with_background_work_keeps_working_children() {
        let (_, mut actor, _) = create_actor();
        let parent_id = spawn_parent_with_child(&mut actor, "parent", "bg-agent").await;

        for event in [
            LifecycleEvent::WorkingEnd,
            LifecycleEvent::BackgroundActivity {
                running_tasks: 1,
                scheduled_tasks: 0,
            },
        ] {
            let (cmd, _) = lifecycle_cmd(&parent_id, event, None);
            actor.handle_command(cmd);
        }

        assert!(
            view_of(&actor, "bg-agent").is_some(),
            "background child kept"
        );
        let parent = view_of(&actor, "parent").expect("parent");
        assert_eq!(parent.activity_detail.as_deref(), Some("1 bg task"));
    }

    #[test]
    fn child_agent_type_keeps_general_purpose_role_visible() {
        assert_eq!(
            child_agent_type(Some("general-purpose")).short_name(),
            "general-purpose"
        );
        assert_eq!(child_agent_type(Some("explore")).short_name(), "explore");
        assert_eq!(
            child_agent_type(Some("claude-code-guide")).short_name(),
            "claude-code-guide"
        );
        assert_eq!(child_agent_type(None).short_name(), "subagent");
        assert_eq!(child_agent_type(Some("  ")).short_name(), "subagent");
    }

    #[tokio::test]
    async fn parent_removal_keeps_process_backed_child() {
        let (_, mut actor, _) = create_actor();
        let parent_pid = std::process::id();
        let parent_id = SessionId::new("lead");
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::RegisterDiscovered {
            session_id: parent_id.clone(),
            pid: parent_pid,
            cwd: std::path::PathBuf::from("/home/user/project"),
            tmux_pane: None,
            harness: atm_core::Harness::ClaudeCode,
            respond_to: tx,
        });
        let (cmd, _) = lifecycle_cmd(
            &parent_id,
            LifecycleEvent::ChildSessionStart {
                id: Some("mate-1".into()),
                role: Some("worker".into()),
            },
            None,
        );
        actor.handle_command(cmd);

        // The teammate turns out to be a real descendant process.
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("failed to spawn sleep process");
        let child_id = SessionId::new("mate-session");
        let (tx, _) = oneshot::channel();
        actor.handle_command(RegistryCommand::RegisterDiscovered {
            session_id: child_id.clone(),
            pid: child.id(),
            cwd: std::path::PathBuf::from("/home/user/project"),
            tmux_pane: None,
            harness: atm_core::Harness::ClaudeCode,
            respond_to: tx,
        });
        assert!(
            view_of(&actor, "mate-1").is_none(),
            "placeholder superseded by the real process"
        );
        assert_eq!(actor.session_count(), 2);

        // The lead ends; the process-backed child must survive, now
        // top-level so the tree still shows it.
        let (cmd, rx) = lifecycle_cmd(
            &parent_id,
            LifecycleEvent::SessionEnd { reason: None },
            None,
        );
        actor.handle_command(cmd);
        assert!(rx.await.unwrap().is_ok());
        let survivor = view_of(&actor, "mate-session").expect("process-backed child kept");
        assert_eq!(survivor.parent_session_id, None);
        assert_eq!(actor.session_count(), 1);

        let _ = child.kill();
        let _ = child.wait();
    }

    #[tokio::test]
    async fn alias_routes_name_only_teammate_events() {
        let (_, mut actor, _) = create_actor();
        let parent_id = spawn_parent_with_child(&mut actor, "lead", "agent-1").await;
        alias(&mut actor, &parent_id, "reviewer", "agent-1");

        // TeammateIdle only names the teammate.
        let (cmd, _) = lifecycle_cmd(&parent_id, LifecycleEvent::Idle, by_name("reviewer"));
        actor.handle_command(cmd);

        assert_eq!(
            actor.session_count(),
            2,
            "no duplicate session for the name"
        );
        let child = view_of(&actor, "agent-1").expect("child");
        assert_eq!(child.status, atm_core::SessionStatus::Idle);
        let parent = view_of(&actor, "lead").expect("lead");
        assert_eq!(parent.status, atm_core::SessionStatus::Working);
    }

    #[tokio::test]
    async fn name_first_child_is_rekeyed_when_alias_arrives() {
        let (_, mut actor, _) = create_actor();
        let parent_id = register_parent(&mut actor, "lead");

        // First sight of the teammate is a name-only event: a placeholder
        // keyed by name and parent appears.
        let (cmd, _) = lifecycle_cmd(&parent_id, LifecycleEvent::Idle, by_name("mate"));
        actor.handle_command(cmd);
        let placeholder = view_of(&actor, "mate@lead").expect("placeholder");
        assert_eq!(placeholder.agent_type, "teammate");
        assert_eq!(placeholder.parent_session_id, Some(parent_id.clone()));

        // Then the spawning call reports its agent id.
        alias(&mut actor, &parent_id, "mate", "a-9");
        // Events keyed by agent id now land on the same session...
        let (cmd, _) = lifecycle_cmd(
            &parent_id,
            LifecycleEvent::ToolCallStart {
                name: Tool::Bash,
                tool_use_id: None,
                input: None,
            },
            by_id("a-9", "worker"),
        );
        actor.handle_command(cmd);
        assert_eq!(actor.session_count(), 2);
        assert_eq!(
            view_of(&actor, "mate@lead").map(|v| v.status),
            Some(atm_core::SessionStatus::Working)
        );
        // ...and SubagentStop by agent id removes it.
        let (cmd, _) = lifecycle_cmd(
            &parent_id,
            LifecycleEvent::ChildSessionEnd {
                id: Some("a-9".into()),
                reason: None,
                last_message: None,
            },
            None,
        );
        actor.handle_command(cmd);
        assert!(view_of(&actor, "mate@lead").is_none());
        assert_eq!(actor.session_count(), 1);
    }

    #[tokio::test]
    async fn unroutable_child_event_never_touches_parent() {
        let (_, mut actor, _) = create_actor();
        // Fill the registry so no child can be created.
        for i in 0..MAX_SESSIONS {
            register_parent(&mut actor, &format!("s-{i}"));
        }
        assert_eq!(actor.session_count(), MAX_SESSIONS);
        let parent_id = SessionId::new("s-0");

        let (cmd, rx) = lifecycle_cmd(&parent_id, LifecycleEvent::Idle, by_name("ghost"));
        actor.handle_command(cmd);
        assert!(rx.await.unwrap().is_ok());
        assert_eq!(actor.session_count(), MAX_SESSIONS);

        // Prove the event was not applied to the parent: a NeedsInput
        // would have flipped it from the idle it starts in.
        let (cmd, _) = lifecycle_cmd(
            &parent_id,
            LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::PermissionGate { tool: Tool::Bash },
            },
            by_name("ghost"),
        );
        actor.handle_command(cmd);
        let parent = view_of(&actor, "s-0").expect("parent");
        assert_eq!(parent.status, atm_core::SessionStatus::Idle);
    }

    #[tokio::test]
    async fn alias_before_registration_is_kept() {
        let (_, mut actor, _) = create_actor();
        let parent_id = register_parent(&mut actor, "lead");
        // PostToolUse(Agent) can be processed before SubagentStart: the
        // pairing must survive until the child registers.
        alias(&mut actor, &parent_id, "reviewer", "a-1");
        assert_eq!(actor.session_count(), 1);

        let (cmd, _) = lifecycle_cmd(
            &parent_id,
            LifecycleEvent::ChildSessionStart {
                id: Some("a-1".into()),
                role: Some("worker".into()),
            },
            None,
        );
        actor.handle_command(cmd);
        let (cmd, _) = lifecycle_cmd(&parent_id, LifecycleEvent::Idle, by_name("reviewer"));
        actor.handle_command(cmd);

        assert_eq!(
            actor.session_count(),
            2,
            "name event must not create a second child"
        );
        assert_eq!(
            view_of(&actor, "a-1").map(|v| v.status),
            Some(atm_core::SessionStatus::Idle)
        );
    }

    #[tokio::test]
    async fn aliases_are_scoped_to_the_parent_session() {
        let (_, mut actor, _) = create_actor();
        let lead_a = spawn_parent_with_child(&mut actor, "lead-a", "agent-a").await;
        let lead_b = spawn_parent_with_child(&mut actor, "lead-b", "agent-b").await;
        alias(&mut actor, &lead_a, "reviewer", "agent-a");
        alias(&mut actor, &lead_b, "reviewer", "agent-b");

        // The same teammate name in two sessions resolves per session.
        let (cmd, _) = lifecycle_cmd(
            &lead_a,
            LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::PermissionGate { tool: Tool::Bash },
            },
            by_name("reviewer"),
        );
        actor.handle_command(cmd);
        assert_eq!(
            view_of(&actor, "agent-a").map(|v| v.status),
            Some(atm_core::SessionStatus::AttentionNeeded)
        );
        assert_eq!(
            view_of(&actor, "agent-b").map(|v| v.status),
            Some(atm_core::SessionStatus::Working)
        );
        assert_eq!(actor.session_count(), 4);

        // An id-tagged event from the wrong session never repaints a
        // child it does not own.
        let (cmd, _) = lifecycle_cmd(&lead_b, LifecycleEvent::Idle, by_id("agent-a", "explore"));
        actor.handle_command(cmd);
        assert_eq!(
            view_of(&actor, "agent-a").map(|v| v.status),
            Some(atm_core::SessionStatus::AttentionNeeded)
        );

        // Ending one lead drops only its aliases; the other still routes.
        let (cmd, rx) = lifecycle_cmd(&lead_a, LifecycleEvent::SessionEnd { reason: None }, None);
        actor.handle_command(cmd);
        assert!(rx.await.unwrap().is_ok());
        let (cmd, _) = lifecycle_cmd(&lead_b, LifecycleEvent::Idle, by_name("reviewer"));
        actor.handle_command(cmd);
        assert_eq!(actor.session_count(), 2);
        assert_eq!(
            view_of(&actor, "agent-b").map(|v| v.status),
            Some(atm_core::SessionStatus::Idle)
        );
    }

    #[tokio::test]
    async fn placeholder_folds_into_registered_child_when_alias_arrives() {
        let (_, mut actor, _) = create_actor();
        let parent_id = spawn_parent_with_child(&mut actor, "lead", "a-1").await;
        // A name-only event lands before the pairing is known.
        let (cmd, _) = lifecycle_cmd(&parent_id, LifecycleEvent::Idle, by_name("reviewer"));
        actor.handle_command(cmd);
        assert_eq!(actor.session_count(), 3, "placeholder created");

        alias(&mut actor, &parent_id, "reviewer", "a-1");
        assert_eq!(actor.session_count(), 2, "placeholder folded away");
        assert!(view_of(&actor, "reviewer@lead").is_none());
        let parent = view_of(&actor, "lead").expect("lead");
        assert_eq!(parent.child_session_ids, vec![SessionId::new("a-1")]);

        let (cmd, _) = lifecycle_cmd(&parent_id, LifecycleEvent::Idle, by_name("reviewer"));
        actor.handle_command(cmd);
        assert_eq!(
            view_of(&actor, "a-1").map(|v| v.status),
            Some(atm_core::SessionStatus::Idle)
        );
        assert_eq!(actor.session_count(), 2);
    }
}
