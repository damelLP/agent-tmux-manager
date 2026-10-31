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

mod children;
mod create;
mod lifecycle;
mod remove;
mod status_line;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use chrono::{DateTime, Utc};
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

use atm_core::{ChildRef, SessionDomain, SessionId, SessionView};

use super::commands::{RegistryCommand, RemovalReason, SessionEvent};
use super::git::{GitContext, GitLookups, GitResult};
use super::infra::SessionInfrastructure;

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

    /// Parent-scoped child references mapped to the child session ids they
    /// resolved to, learned from aliases.
    child_refs: HashMap<(SessionId, ChildRef), SessionId>,

    /// When each `(session_id, pid)` ended: its `SessionEnd` removed it, or
    /// a new session's `SessionStart` in that pid ended it. Until
    /// [`ENDED_SESSION_TTL`] passes, a status line for a pair is dropped,
    /// and so is a `SessionEnd` that no longer names the pid's row.
    ended_sessions: HashMap<(SessionId, u32), DateTime<Utc>>,

    /// Git-context lookups running off this task.
    git_lookups: GitLookups,

    /// Where finished git-context lookups report back.
    git_results: mpsc::UnboundedReceiver<GitResult>,
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
        Self::with_git_resolver(receiver, event_publisher, resolve_git_context)
    }

    /// Creates a registry actor whose git-context lookups call `resolve`.
    fn with_git_resolver(
        receiver: mpsc::Receiver<RegistryCommand>,
        event_publisher: broadcast::Sender<SessionEvent>,
        resolve: impl Fn(&str) -> GitContext + Send + Sync + 'static,
    ) -> Self {
        let (git_lookups, git_results) = GitLookups::new(resolve);
        Self {
            receiver,
            sessions_by_pid: HashMap::new(),
            session_id_to_pid: HashMap::new(),
            event_publisher,
            child_refs: HashMap::new(),
            ended_sessions: HashMap::new(),
            git_lookups,
            git_results,
        }
    }

    /// Runs the actor event loop.
    ///
    /// Processes commands and finished git lookups until the command channel
    /// closes (all senders dropped). A lookup still running doesn't delay that.
    /// This is the main entry point - call this in a spawned task.
    pub async fn run(mut self) {
        info!("Registry actor starting");

        loop {
            tokio::select! {
                cmd = self.receiver.recv() => match cmd {
                    Some(cmd) => self.handle_command(cmd),
                    None => break,
                },
                // `git_lookups` holds a sender, so this channel stays open.
                Some((cwd, context)) = self.git_results.recv() => {
                    self.apply_git_result(cwd, context);
                }
            }
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
                pid,
                data,
                harness,
                respond_to,
            } => {
                let result = self.handle_update_from_status_line(session_id, pid, data, harness);
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

    /// Publishes the current view of the session stored under `pid`.
    fn publish_updated(&self, pid: u32) {
        if let Some((session, _)) = self.sessions_by_pid.get(&pid) {
            let _ = self.event_publisher.send(SessionEvent::Updated {
                session: Box::new(SessionView::from_domain(session)),
            });
        }
    }

    /// Publishes that `session_id` left the registry.
    fn publish_removed(&self, session_id: SessionId, reason: RemovalReason) {
        let _ = self
            .event_publisher
            .send(SessionEvent::Removed { session_id, reason });
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

    /// Refreshes git info (branch, worktree) for all sessions.
    ///
    /// Detects branch switches that happen without a working directory change
    /// (e.g., `git checkout other-branch` in the same directory).
    fn handle_refresh_git_info(&mut self) {
        let now = Instant::now();
        let cwds: HashSet<&str> = self
            .sessions_by_pid
            .values()
            .filter_map(|(session, _)| session.working_directory.as_deref())
            .collect();
        for cwd in cwds {
            self.git_lookups.request(cwd, now);
        }

        for cwd in self.git_lookups.newly_stuck(now) {
            warn!(cwd = %cwd, "Git lookup is stuck; keeping its slot until it returns");
        }
    }

    /// Applies a finished git-context lookup to every session in `cwd`,
    /// children included. A session that has left `cwd` keeps its fields, and
    /// so does every session when the lookup panicked.
    fn apply_git_result(&mut self, cwd: String, context: Option<GitContext>) {
        self.git_lookups.finished(&cwd);
        let Some((new_project_root, new_wt_path, new_wt_branch)) = context else {
            warn!(cwd = %cwd, "Git lookup panicked; the next refresh retries");
            return;
        };
        let mut updated_pids = Vec::new();

        for (pid, (session, _)) in self.sessions_by_pid.iter_mut() {
            if session.working_directory.as_deref() != Some(cwd.as_str()) {
                continue;
            }

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
                session.project_root = new_project_root.clone();
                session.worktree_path = new_wt_path.clone();
                session.worktree_branch = new_wt_branch.clone();
                updated_pids.push(*pid);
            }
        }

        // Publish in loop order once the loop's borrow has ended
        for pid in updated_pids {
            self.publish_updated(pid);
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

/// Resolves the git context of `cwd`: its project root, worktree path and
/// worktree branch. The worktree fields are only set when `cwd` is itself a
/// repo or worktree root (see `crate::project::resolve_worktree_info`).
/// This reads files under `.git` and can block, so it only runs through
/// [`GitLookups`].
fn resolve_git_context(cwd: &str) -> GitContext {
    let project_root = crate::project::resolve_project_root(cwd);
    let (worktree_path, worktree_branch) = crate::project::resolve_worktree_info(cwd);
    (project_root, worktree_path, worktree_branch)
}

/// How long after a session ends in a process (see `ended_sessions`) its
/// status lines from that process are dropped, and a `SessionEnd` that no
/// longer names the process's row keeps it. Claude Code 2.1.310 sends a
/// status line ~30 ms after `SessionEnd` on `/clear`; 10 s leaves a wide
/// margin for a loaded host.
const ENDED_SESSION_TTL: chrono::Duration = chrono::Duration::seconds(10);

/// Whether an ended-session marker set at `ended_at` is still live at
/// `now`. A clock that went backwards expires it, which restores the
/// behaviour from before the marker existed.
fn marker_live(ended_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    let age = now.signed_duration_since(ended_at);
    age >= chrono::Duration::zero() && age < ENDED_SESSION_TTL
}
