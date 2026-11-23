//! Reusable session control operations, independent of CLI environment and output.

mod command;
mod pane;
mod panes;
mod peek;
mod reply;
pub mod reservation;
mod roles;
mod send;
pub mod service;
mod spawn;
pub(crate) mod windows;
pub mod workspace;

pub use command::{build_spawn_command, launch_policy, shell_quote, spawn_env_override};
pub use pane::{interrupt, kill};
pub use panes::session_panes;
pub use peek::peek;
#[cfg(test)]
pub use reply::reply;
pub use reply::ReplyOutcome;
pub use send::resolve_pane_id;
#[cfg(test)]
pub use send::send;
pub use spawn::{
    prepare_spawn, prepare_spawn_largest, split_spawn, type_spawn_command, SpawnContext,
    SPAWN_ENTER_FAILURE_CONTEXT,
};

/// Failures resolving or controlling a session's tmux pane.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("No session matching '{target}'")]
    NoSession { target: String },
    #[error("Session {session} has no tmux pane")]
    NoPane { session: String },
    #[error("Ambiguous target '{target}' matches {count} sessions: {ids}")]
    AmbiguousTarget {
        target: String,
        count: usize,
        ids: String,
    },
    #[error("Refusing to send to {pane}: {reason}")]
    SendRefused { pane: String, reason: &'static str },
    #[error("Refusing to send to {pane}: agent {agent} is clearing; try again in a moment")]
    AgentClearing {
        pane: String,
        agent: atm_org::AgentId,
    },
    #[error("pane {pane}: agent {agent} is quarantined; restart atmd and reconcile")]
    PaneQuarantined {
        pane: String,
        agent: atm_org::AgentId,
    },
    #[error("organisation state is unavailable for pane {pane}; refusing input")]
    OrganisationUnavailable { pane: String },
    #[error("{context}")]
    Tmux {
        context: String,
        #[source]
        source: atm_tmux::TmuxError,
    },
}

#[cfg(test)]
pub(crate) mod legacy_survey_test;
#[cfg(test)]
mod operation_tests;
#[cfg(test)]
mod tests;
