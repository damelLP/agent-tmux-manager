//! Child sessions: creation, aliases, background activity and detaching.

use atm_core::{
    AgentType, BackgroundActivity, ChildAlias, ChildRef, LifecycleContext, LifecycleEvent,
    SessionDomain, SessionId,
};

use super::RegistryActor;
use crate::registry::commands::RemovalReason;

impl RegistryActor {
    /// Child bookkeeping for an event received on `parent_pid`: learns
    /// name→id aliases, tracks `ChildSessionStart`/`End`, and applies an
    /// event emitted by an in-process child to that child.
    ///
    /// Returns `true` when the event was routed to a child.
    pub(super) fn prepare_child_event(
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
        if let Some(alias) = &context.child_alias {
            self.register_child_alias(&parent_id, alias);
        }
        match event {
            LifecycleEvent::ChildSessionStart {
                id: Some(agent_id),
                role,
            } => {
                let child_id = self.child_id_for(&parent_id, &ChildRef::Id(agent_id.clone()));
                let agent_type = AgentType::for_child(role.as_deref());
                self.ensure_child_session(parent_pid, child_id, agent_type, harness);
            }
            LifecycleEvent::ChildSessionEnd { id: Some(agent_id) } => {
                let child_id = self.child_id_for(&parent_id, &ChildRef::Id(agent_id.clone()));
                let _ = self.handle_remove(child_id, RemovalReason::SessionEnded);
            }
            _ => {}
        }
        let Some(child) = &context.child else {
            return false;
        };
        let child_id = self.child_id_for(&parent_id, &child.reference);
        let agent_type = child.agent_type.clone();
        if let Some(pid) = self.ensure_child_session(parent_pid, child_id, agent_type, harness) {
            self.apply_to_session(pid, event, None);
        }
        true
    }

    /// Resolves a vendor child reference to its session id under `parent`:
    /// the id an alias mapped it to, else the reference's own default id.
    fn child_id_for(&self, parent: &SessionId, reference: &ChildRef) -> SessionId {
        self.child_refs
            .get(&(parent.clone(), reference.clone()))
            .cloned()
            .unwrap_or_else(|| match reference {
                ChildRef::Id(id) => SessionId::new(id),
                ChildRef::Name(name) => SessionId::scoped(name, parent),
            })
    }

    /// The in-process (synthetic, no real pid) session with this id, if any.
    fn synthetic_child(&self, id: &SessionId) -> Option<&SessionDomain> {
        let (session, infra) = self.sessions_by_pid.get(self.session_id_to_pid.get(id)?)?;
        infra.pid.is_none().then_some(session)
    }

    /// Returns the pid of `child_id`, creating it as an in-process child of
    /// `parent_pid` when it does not exist yet.
    pub(super) fn ensure_child_session(
        &mut self,
        parent_pid: u32,
        child_id: SessionId,
        agent_type: AgentType,
        harness: atm_core::Harness,
    ) -> Option<u32> {
        if let Some(pid) = self.session_id_to_pid.get(&child_id) {
            return Some(*pid);
        }
        let (parent, _) = self.sessions_by_pid.get(&parent_pid)?;
        let mut child = SessionDomain::new(child_id.clone(), agent_type, parent.model);
        child.harness = harness;
        child.model_display_override = parent.model_display_override.clone();
        child.tmux_pane = parent.tmux_pane.clone();
        child.working_directory = parent.working_directory.clone();
        child.project_root = parent.project_root.clone();
        child.worktree_path = parent.worktree_path.clone();
        child.worktree_branch = parent.worktree_branch.clone();
        child.parent_session_id = Some(parent.id.clone());
        child.apply_lifecycle_event(&LifecycleEvent::WorkingStart);
        // Without a real pid the registry assigns a synthetic one that
        // `set_pid` rejects, so `infra.pid` stays `None` and stale-process
        // cleanup never sweeps the child.
        self.handle_register(child, None).ok()?;
        let child_pid = self.session_id_to_pid.get(&child_id).copied()?;
        if let Some((parent, _)) = self.sessions_by_pid.get_mut(&parent_pid) {
            parent.child_session_ids.push(child_id);
        }
        self.publish_updated(child_pid);
        self.publish_updated(parent_pid);
        Some(child_pid)
    }

    /// Maps a child's name and agent id to one child session under
    /// `parent`, retiring a name-only placeholder the alias supersedes.
    pub(super) fn register_child_alias(&mut self, parent: &SessionId, alias: &ChildAlias) {
        let by_id = ChildRef::Id(alias.id.clone());
        let by_name = ChildRef::Name(alias.name.clone());
        let target = self.child_id_for(parent, &by_id);
        let placeholder = self.child_id_for(parent, &by_name);
        if placeholder != target && self.synthetic_child(&placeholder).is_some() {
            let _ = self.handle_remove(placeholder, RemovalReason::Upgraded);
        }
        for reference in [by_name, by_id] {
            self.child_refs
                .insert((parent.clone(), reference), target.clone());
        }
    }

    /// After a parent `Stop`: sweeps in-process children still marked
    /// working once nothing runs in the background (their `SubagentStop`
    /// never arrived), and shows remaining background work on the parent.
    pub(super) fn apply_background_activity(
        &mut self,
        pid: u32,
        activity: Option<BackgroundActivity>,
    ) {
        let Some(activity) = activity else {
            return;
        };
        if activity.is_quiet() {
            let child_ids = self
                .sessions_by_pid
                .get(&pid)
                .map(|(parent, _)| parent.child_session_ids.clone())
                .unwrap_or_default();
            for id in child_ids {
                let working = self
                    .synthetic_child(&id)
                    .is_some_and(|child| child.status == atm_core::SessionStatus::Working);
                if working {
                    let _ = self.handle_remove(id, RemovalReason::SessionEnded);
                }
            }
        }
        let Some(summary) = activity.summary() else {
            return;
        };
        let Some((session, _)) = self.sessions_by_pid.get_mut(&pid) else {
            return;
        };
        if session.status != atm_core::SessionStatus::Idle {
            return;
        }
        session.current_activity = Some(atm_core::ActivityDetail::with_context(&summary));
        self.publish_updated(pid);
    }

    /// Unlinks a session that just left the registry from its children and
    /// parent, removing in-process children with it.
    pub(super) fn detach_removed_session(
        &mut self,
        session: &SessionDomain,
        reason: RemovalReason,
    ) {
        for id in &session.child_session_ids {
            let _ = self.handle_remove(id.clone(), reason);
        }
        if let Some(pid) = session
            .parent_session_id
            .as_ref()
            .and_then(|id| self.session_id_to_pid.get(id))
            .copied()
        {
            if let Some((parent, _)) = self.sessions_by_pid.get_mut(&pid) {
                parent.child_session_ids.retain(|id| id != &session.id);
            }
            self.publish_updated(pid);
        }
        self.child_refs
            .retain(|(parent, _), child| parent != &session.id && child != &session.id);
    }
}
