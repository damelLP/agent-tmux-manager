//! Applying status lines to stored sessions.

use std::time::Instant;

use chrono::Utc;
use tracing::{debug, info};

use atm_core::{SessionDomain, SessionId, StatusLineData};

use super::RegistryActor;
use crate::registry::commands::RegistryError;
use crate::registry::git::GitLookups;

impl RegistryActor {
    /// Handles status line update.
    ///
    /// With PID as primary key, the logic is simplified:
    /// - If we have a PID, look up by PID and update (or create) the session
    /// - If no PID, fall back to session_id lookup
    pub(super) fn handle_update_from_status_line(
        &mut self,
        session_id: SessionId,
        status_pid: Option<u32>,
        data: StatusLineData,
        harness: atm_core::Harness,
    ) -> Result<(), RegistryError> {
        // Primary lookup: by PID (preferred)
        if let Some(pid) = status_pid {
            if pid != 0 {
                return self.update_or_create_by_pid(pid, session_id, data, harness);
            }
        }

        // Fallback: lookup by session_id (when no PID available)
        if let Some(&pid) = self.session_id_to_pid.get(&session_id) {
            if let Some((session, _)) = self.sessions_by_pid.get_mut(&pid) {
                apply_status_line(session, &data, &session_id, pid, &mut self.git_lookups);

                debug!(
                    session_id = %session_id,
                    pid = pid,
                    cost = %session.cost,
                    "Session updated from status line (by session_id)"
                );

                self.publish_updated(pid);
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
    /// A late status line for a session that just ended in this PID is
    /// dropped, so it can't bring the ended session back.
    fn update_or_create_by_pid(
        &mut self,
        pid: u32,
        session_id: SessionId,
        data: StatusLineData,
        harness: atm_core::Harness,
    ) -> Result<(), RegistryError> {
        if self.ended_in(&session_id, pid, Utc::now()) {
            debug!(
                session_id = %session_id,
                pid = pid,
                "Status line for a session that ended in this process, ignoring"
            );
            return Ok(());
        }

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

        if let Some((session, _)) = self.sessions_by_pid.get_mut(&pid) {
            let log_id = session.id.clone();
            apply_status_line(session, &data, &log_id, pid, &mut self.git_lookups);

            debug!(
                session_id = %session_id,
                pid = pid,
                cost = %session.cost,
                "Session updated from status line"
            );

            self.publish_updated(pid);
        } else {
            // Session doesn't exist - create it
            let mut session = SessionDomain::from_status_line(&data);
            session.harness = harness;

            self.insert_new_session(pid, session)?;

            info!(
                session_id = %session_id,
                pid = pid,
                "Session auto-registered from status line"
            );

            self.publish_updated(pid);
        }

        Ok(())
    }
}

/// Applies a status line to a stored session. Then requests a new git context
/// if the project root is unset or the working directory changed, logging
/// `log_id` on a change. The old fields stay until the lookup lands.
fn apply_status_line(
    session: &mut SessionDomain,
    data: &StatusLineData,
    log_id: &SessionId,
    pid: u32,
    git_lookups: &mut GitLookups,
) {
    let cwd_changed = session.update_from_status_line(data);

    // Resolve project/worktree if not yet set, or if cwd changed
    if session.project_root.is_none() || cwd_changed {
        if let Some(ref cwd) = session.working_directory {
            if cwd_changed {
                info!(
                    session_id = %log_id,
                    pid = pid,
                    new_cwd = %cwd,
                    "Working directory changed, re-resolving git info"
                );
            }
            git_lookups.request(cwd, Instant::now());
        }
    }
}
