//! Removing sessions, ended-session markers and stale cleanup.

use chrono::{DateTime, Utc};
use tracing::{debug, info, warn};

use atm_core::SessionId;

use super::{marker_live, RegistryActor};
use crate::registry::commands::{RegistryError, RemovalReason};

impl RegistryActor {
    /// Handles removing a session by session_id.
    pub(super) fn handle_remove(
        &mut self,
        session_id: SessionId,
        reason: RemovalReason,
    ) -> Result<(), RegistryError> {
        let pid = match self.session_id_to_pid.get(&session_id) {
            Some(p) => *p,
            None => return Err(RegistryError::SessionNotFound(session_id)),
        };
        self.remove_and_announce(pid, session_id, reason);
        Ok(())
    }

    /// Handles removing a session by PID.
    pub(super) fn handle_remove_by_pid(
        &mut self,
        pid: u32,
        reason: RemovalReason,
    ) -> Result<(), RegistryError> {
        let session_id = match self.sessions_by_pid.get(&pid) {
            Some((session, _)) => session.id.clone(),
            None => {
                return Err(RegistryError::SessionNotFound(SessionId::new(format!(
                    "pid-{pid}"
                ))));
            }
        };
        self.remove_and_announce(pid, session_id, reason);
        Ok(())
    }

    /// Removes the session at `pid`, then logs and publishes its removal
    /// as `session_id`.
    fn remove_and_announce(&mut self, pid: u32, session_id: SessionId, reason: RemovalReason) {
        self.remove_entry(pid, &session_id, reason);

        info!(
            session_id = %session_id,
            pid = pid,
            reason = %reason,
            remaining_sessions = self.sessions_by_pid.len(),
            "Session removed"
        );

        self.publish_removed(session_id, reason);
    }

    /// Removes the row at `pid` and `session_id`'s index entry if it points
    /// at `pid`, then detaches the removed session from its parent and
    /// children.
    fn remove_entry(&mut self, pid: u32, session_id: &SessionId, reason: RemovalReason) {
        let removed = self.sessions_by_pid.remove(&pid);
        self.unindex(session_id, pid);
        if let Some((session, _)) = removed {
            self.detach_removed_session(&session, reason);
        }
    }

    /// Drops `session_id`'s index entry only if it points at `pid`. Another
    /// process can report the same id, and its row keeps the entry.
    pub(super) fn unindex(&mut self, session_id: &SessionId, pid: u32) {
        if self.session_id_to_pid.get(session_id) == Some(&pid) {
            self.session_id_to_pid.remove(session_id);
        }
    }

    /// Marks `session_id` as ended in `pid` at `now`, first dropping the
    /// markers that have expired, which bounds the map.
    pub(super) fn record_session_end(
        &mut self,
        session_id: SessionId,
        pid: u32,
        now: DateTime<Utc>,
    ) {
        self.ended_sessions
            .retain(|_, ended_at| marker_live(*ended_at, now));
        self.ended_sessions.insert((session_id, pid), now);
    }

    /// Ends `session_id` in `pid` at `now`, as its `SessionEnd` does: marks
    /// it ended there and removes the pid's row, detaching its children.
    pub(super) fn end_session_in(
        &mut self,
        session_id: SessionId,
        pid: u32,
        now: DateTime<Utc>,
    ) -> Result<(), RegistryError> {
        self.record_session_end(session_id, pid, now);
        self.handle_remove_by_pid(pid, RemovalReason::SessionEnded)
    }

    /// Whether `session_id` ended in `pid` within [`ENDED_SESSION_TTL`](super::ENDED_SESSION_TTL)
    /// before `now`.
    pub(super) fn ended_in(&self, session_id: &SessionId, pid: u32, now: DateTime<Utc>) -> bool {
        let ended_at = self.ended_sessions.get(&(session_id.clone(), pid));
        ended_at.is_some_and(|ended_at| marker_live(*ended_at, now))
    }

    /// Handles cleanup of dead-process sessions.
    ///
    /// Removes sessions whose agent process has terminated
    /// (PID no longer exists or was reused by a different process).
    pub(super) fn handle_cleanup_stale(&mut self) {
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

            self.remove_entry(pid, &session_id, RemovalReason::ProcessDied);

            // Use warn! so it shows up without RUST_LOG=debug
            warn!(
                session_id = %session_id,
                reason = %RemovalReason::ProcessDied,
                details = %log_details,
                "Session removed by cleanup"
            );

            self.publish_removed(session_id, RemovalReason::ProcessDied);
        }
    }
}
