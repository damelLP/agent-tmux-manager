//! Applying adapter lifecycle events to sessions.

use chrono::Utc;
use tracing::{debug, info};

use atm_core::{AgentType, LifecycleContext, LifecycleEvent, SessionId};

use super::create::build_session_from_pid;
use super::RegistryActor;
use crate::discovery::read_parent_session_id;
use crate::registry::commands::RegistryError;

impl RegistryActor {
    /// Handles applying a vendor-neutral lifecycle event to a session.
    ///
    /// With PID as primary key, we can look up by PID when available.
    ///
    /// Special cases:
    /// - An untagged `SessionEnd` immediately removes the session from the
    ///   registry, and marks it ended in that process (see `ended_sessions`).
    ///   An untagged `SessionStart` for the same id and process clears the mark.
    /// - An untagged `SessionStart` with a new id ends the session its
    ///   process's row held, as that session's `SessionEnd` would, and starts
    ///   a fresh row. A `SessionEnd` for the old id that arrives afterwards
    ///   keeps the new row.
    /// - `ChildSessionStart`/`ChildSessionEnd` create and remove in-process children.
    pub(super) fn handle_apply_lifecycle_event(
        &mut self,
        session_id: SessionId,
        event: LifecycleEvent,
        harness: atm_core::Harness,
        pid: Option<u32>,
        tmux_pane: Option<String>,
        context: LifecycleContext,
    ) -> Result<(), RegistryError> {
        let target_pid = pid.or_else(|| self.session_id_to_pid.get(&session_id).copied());

        // Untagged SessionEnd: remove session immediately.
        if context.child.is_none() && matches!(event, LifecycleEvent::SessionEnd { .. }) {
            if let Some(p) = target_pid {
                if let Some((session, _)) = self.sessions_by_pid.get(&p) {
                    let now = Utc::now();
                    // A SessionStart already ended `session_id` in this
                    // process, so the row is a newer session's.
                    if session.id != session_id && self.ended_in(&session_id, p, now) {
                        info!(
                            session_id = %session_id,
                            pid = p,
                            current_id = %session.id,
                            "SessionEnd for a session this process moved on from, keeping the row"
                        );
                        return Ok(());
                    }
                    info!(
                        session_id = %session_id,
                        pid = p,
                        "SessionEnd received, removing session"
                    );
                    return self.end_session_in(session_id, p, now);
                }
            }

            debug!(
                session_id = %session_id,
                "SessionEnd for non-existent session (already cleaned up or never created)"
            );
            return Ok(());
        }

        // A session started again in the process it ended in (an in-process
        // resume) takes its status lines again.
        let starts_session =
            context.child.is_none() && matches!(event, LifecycleEvent::SessionStart { .. });
        if starts_session {
            if let Some(p) = target_pid {
                self.ended_sessions.remove(&(session_id.clone(), p));
            }
        }

        // Pending → real upgrade: a session discovered via /proc starts
        // life as `pending-{pid}`. The first vendor-adapter event with
        // a real session_id is our signal to reconcile, mirroring the
        // Claude status-line upgrade path. Limited to pending → real
        // so that ordinary pi events (which carry session_id on every
        // frame) don't thrash the index. A real id is replaced only by an
        // untagged SessionStart, below.
        if let Some(p) = target_pid {
            if let Some((existing, _)) = self.sessions_by_pid.get(&p) {
                let current_id = existing.id.clone();
                if current_id.is_pending() && !session_id.is_pending() && current_id != session_id {
                    self.reconcile_session_id(p, current_id, session_id.clone());
                } else if starts_session && !session_id.is_pending() && current_id != session_id {
                    // A new session took over the process (`/clear`, an
                    // in-process resume), so the one its row held ended
                    // there. End it as its SessionEnd would; the event
                    // then starts a fresh row for the pid below. (A
                    // pid-less event finds its row through the id index,
                    // so its row already has its id.)
                    info!(
                        session_id = %current_id,
                        pid = p,
                        new_id = %session_id,
                        "SessionStart of a new session, ending the session this process's row held"
                    );
                    self.end_session_in(current_id, p, Utc::now())?;
                }
            }
        }

        let existing = target_pid.filter(|p| self.sessions_by_pid.contains_key(p));
        let Some(p) = existing.or(pid.filter(|p| *p != 0)) else {
            debug!(
                session_id = %session_id,
                event = ?event,
                "Lifecycle event for non-existent session without PID, ignoring"
            );
            return Ok(());
        };
        if existing.is_none() {
            // Session doesn't exist yet - this is normal due to race conditions.
            // With PID as primary key, we can create the session now if we have a PID.
            debug!(
                session_id = %session_id,
                pid = p,
                event = ?event,
                "Creating session from lifecycle event"
            );
            // Read cwd from /proc/{pid}/cwd so a session created
            // via a vendor adapter event gets grouped under the
            // right project / branch once its git lookup lands. Without
            // this, lifecycle-event-created sessions stay in the
            // "Other" tree bucket because working_directory is None.
            let proc_cwd = std::fs::read_link(format!("/proc/{p}/cwd")).ok();
            let session = build_session_from_pid(
                session_id.clone(),
                AgentType::GeneralPurpose,
                atm_core::Model::Unknown,
                harness,
                tmux_pane.clone(),
                proc_cwd,
                read_parent_session_id(p, harness),
            );
            self.insert_new_session(p, session)?;
        }

        if !self.prepare_child_event(p, &event, harness, &context) {
            self.apply_to_session(p, &event, tmux_pane);
            self.apply_background_activity(p, context.background_activity);
        }
        Ok(())
    }

    /// Applies `event` to the session stored under `pid` and publishes it.
    pub(super) fn apply_to_session(
        &mut self,
        pid: u32,
        event: &LifecycleEvent,
        tmux_pane: Option<String>,
    ) {
        let Some((session, _)) = self.sessions_by_pid.get_mut(&pid) else {
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
        self.publish_updated(pid);
    }
}
