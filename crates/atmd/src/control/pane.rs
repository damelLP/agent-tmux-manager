//! Legacy pane termination and interruption, without CLI environment or output.

use atm_core::SessionView;
use atm_tmux::TmuxClient;

use super::{resolve_pane_id, ControlError};

/// Resolve the target, close its pane and return the affected pane id.
pub async fn kill(
    client: &(impl TmuxClient + ?Sized),
    sessions: &[SessionView],
    target: &str,
) -> Result<String, ControlError> {
    let pane_id = resolve_pane_id(sessions, target)?;
    client
        .kill_pane(&pane_id)
        .await
        .map_err(|source| ControlError::Tmux {
            context: format!("Failed to kill pane {pane_id}"),
            source,
        })?;
    Ok(pane_id)
}

/// Resolve the target, send Ctrl+C and return the affected pane id.
pub async fn interrupt(
    client: &(impl TmuxClient + ?Sized),
    sessions: &[SessionView],
    target: &str,
) -> Result<String, ControlError> {
    let pane_id = resolve_pane_id(sessions, target)?;
    // C-c sends SIGINT to the foreground process in the pane
    client
        .send_keys(&pane_id, "C-c")
        .await
        .map_err(|source| ControlError::Tmux {
            context: format!("Failed to interrupt pane {pane_id}"),
            source,
        })?;
    Ok(pane_id)
}
