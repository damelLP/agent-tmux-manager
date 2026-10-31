//! Creating sessions: registration, discovery and session-id changes.

use std::path::PathBuf;
use std::time::Instant;

use tracing::{debug, info, warn};

use atm_core::{AgentType, SessionDomain, SessionId};

use super::{RegistryActor, MAX_SESSIONS};
use crate::discovery::read_parent_session_id;
use crate::registry::commands::{RegistryError, RemovalReason, SessionEvent};
use crate::registry::infra::SessionInfrastructure;

impl RegistryActor {
    /// Handles session registration.
    ///
    /// Note: This is now primarily used for testing. Most sessions are
    /// registered via `handle_register_discovered`, status line updates or
    /// adapter events.
    /// Without a PID, this creates a session that cannot be looked up by PID.
    pub(super) fn handle_register(
        &mut self,
        session: SessionDomain,
        pid: Option<u32>,
    ) -> Result<(), RegistryError> {
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
        self.insert_new_session(pid, session)?;

        info!(
            session_id = %session_id,
            pid = pid,
            agent_type = ?agent_type,
            total_sessions = self.sessions_by_pid.len(),
            "Session registered"
        );

        Ok(())
    }

    /// Stores `session` under a `pid` the registry does not hold yet.
    ///
    /// Every new session enters the registry here, so each creation path
    /// enforces the capacity limit, records the pid, inserts into both
    /// indexes and publishes `Registered`. It does not check whether another
    /// pid already holds `session.id`.
    pub(super) fn insert_new_session(
        &mut self,
        pid: u32,
        session: SessionDomain,
    ) -> Result<(), RegistryError> {
        if self.sessions_by_pid.len() >= MAX_SESSIONS {
            warn!(
                session_id = %session.id,
                current = self.sessions_by_pid.len(),
                max = MAX_SESSIONS,
                "Registry is full, rejecting new session"
            );
            return Err(RegistryError::RegistryFull { max: MAX_SESSIONS });
        }

        let session_id = session.id.clone();
        let agent_type = session.agent_type.clone();

        // Resolve project/worktree if not already set
        if session.project_root.is_none() {
            if let Some(cwd) = &session.working_directory {
                self.git_lookups.request(cwd, Instant::now());
            }
        }

        let mut infra = SessionInfrastructure::new();
        infra.set_pid(pid);

        self.sessions_by_pid.insert(pid, (session, infra));
        self.session_id_to_pid.insert(session_id.clone(), pid);

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
    /// with full data when status line updates or adapter events arrive.
    ///
    /// With PID as primary key, if a session already exists for this PID,
    /// we update its session_id rather than creating a duplicate.
    pub(super) fn handle_register_discovered(
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
            let session_id = if !old_id.is_pending() && session_id.is_pending() {
                old_id.clone()
            } else {
                session_id
            };

            // Update session_id to match the new discovery
            existing_session.id = session_id.clone();

            self.git_lookups.request(&cwd_str, Instant::now());
            existing_session.working_directory = Some(cwd_str);
            if tmux_pane.is_some() {
                existing_session.tmux_pane = tmux_pane;
            }

            info!(
                old_id = %old_id,
                new_id = %session_id,
                pid = pid,
                "Re-discovered existing session, refreshing git info (metadata preserved)"
            );

            self.publish_updated(pid);
            self.rename_links(&old_id, &session_id);

            // Update the session_id index
            self.unindex(&old_id, pid);
            self.session_id_to_pid.insert(session_id, pid);

            return Ok(());
        }

        // Create minimal session with defaults (genuinely new process).
        // Agent type and model will be updated when the status line or
        // adapter events arrive.
        // Harness tag comes from whichever discoverer matched (Claude, pi,
        // Codex, future); subsequent adapter events can refine but not change
        // identity.
        use atm_core::Model;
        let session = build_session_from_pid(
            session_id.clone(),
            AgentType::GeneralPurpose,
            Model::Unknown,
            harness,
            tmux_pane,
            Some(cwd),
            read_parent_session_id(pid, harness),
        );
        self.insert_new_session(pid, session)?;

        info!(
            session_id = %session_id,
            pid = pid,
            total_sessions = self.sessions_by_pid.len(),
            "Discovered session registered"
        );

        // Also publish an initial Updated event so TUI shows it
        self.publish_updated(pid);

        Ok(())
    }

    /// Renames an existing session at `pid` from `old_id` to `new_id`,
    /// updates the session_id index, and publishes the
    /// `Removed{Upgraded}` + `Registered` event pair so TUI subscribers
    /// see the id transition cleanly. Links to `old_id` follow the rename.
    ///
    /// Used by both the Claude status-line path and any vendor adapter
    /// whose first events arrive before the real session_id is known.
    /// Without this, a session discovered as `pending-{pid}`
    /// stays at that id forever even after real adapter events fire.
    ///
    /// No-op if the session is not present at `pid`.
    pub(super) fn reconcile_session_id(&mut self, pid: u32, old_id: SessionId, new_id: SessionId) {
        if old_id == new_id {
            return;
        }
        let Some((session, _)) = self.sessions_by_pid.get_mut(&pid) else {
            return;
        };
        session.id = new_id.clone();
        let agent_type = session.agent_type.clone();

        self.unindex(&old_id, pid);
        self.session_id_to_pid.insert(new_id.clone(), pid);

        info!(
            old_id = %old_id,
            new_id = %new_id,
            pid = pid,
            "Session ID upgraded"
        );

        self.publish_removed(old_id.clone(), RemovalReason::Upgraded);
        let _ = self.event_publisher.send(SessionEvent::Registered {
            session_id: new_id.clone(),
            agent_type,
        });
        self.rename_links(&old_id, &new_id);
    }

    /// Points every parent/child link to `old` at `new` after a session
    /// rename, then publishes each session whose links changed.
    fn rename_links(&mut self, old: &SessionId, new: &SessionId) {
        // A row still holds `old` when nothing was renamed, or when another
        // row shares the id and may own these links; shared ids are the
        // user's open question (#97), so they stay as they are.
        if self.sessions_by_pid.values().any(|(s, _)| s.id == *old) {
            return;
        }
        let mut relinked = Vec::new();
        for (pid, (session, _)) in &mut self.sessions_by_pid {
            let links = (session.parent_session_id.iter_mut())
                .chain(&mut session.child_session_ids)
                .filter(|id| *id == old);
            let mut changed = false;
            for id in links {
                *id = new.clone();
                changed = true;
            }
            if changed {
                relinked.push(*pid);
            }
        }
        let rename = |id: SessionId| if id == *old { new.clone() } else { id };
        self.child_refs = std::mem::take(&mut self.child_refs)
            .into_iter()
            .map(|((parent, reference), child)| ((rename(parent), reference), rename(child)))
            .collect();
        for pid in relinked {
            self.publish_updated(pid);
        }
    }
}

/// Builds a fresh `SessionDomain` for a newly-observed PID.
///
/// Used by both the /proc-discovery path (`handle_register_discovered`)
/// and the create-on-event path inside `handle_apply_lifecycle_event`.
/// `insert_new_session` starts the lookup of the project / worktree fields,
/// which fill in shortly after the session first appears.
///
/// When `cwd` is `None` (e.g., `/proc/{pid}/cwd` read failed) the
/// working directory stays unset and no lookup runs.
/// `parent_session_id` nests a separate-process child (e.g. a tmux-mode
/// teammate) under the session that spawned it.
pub(super) fn build_session_from_pid(
    session_id: SessionId,
    agent_type: AgentType,
    model: atm_core::Model,
    harness: atm_core::Harness,
    tmux_pane: Option<String>,
    cwd: Option<PathBuf>,
    parent_session_id: Option<SessionId>,
) -> SessionDomain {
    let mut session = SessionDomain::new(session_id, agent_type, model);
    session.harness = harness;
    session.tmux_pane = tmux_pane;
    session.parent_session_id = parent_session_id;
    session.working_directory = cwd.map(|cwd| cwd.to_string_lossy().to_string());
    session
}
