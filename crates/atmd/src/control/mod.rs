//! Reusable session control operations, independent of CLI environment and output.

mod command;
mod pane;
mod peek;
mod reply;
pub mod reservation;
mod send;
pub mod service;
mod spawn;

pub use command::{build_spawn_command, launch_policy, shell_quote, spawn_env_override};
pub use pane::{interrupt, kill};
pub use peek::peek;
pub use reply::{reply, ReplyOutcome};
pub use send::{resolve_pane_id, send};
pub use spawn::{prepare_spawn, split_spawn, type_spawn_command, SpawnContext};

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
mod operation_tests;
#[cfg(test)]
mod tests;
