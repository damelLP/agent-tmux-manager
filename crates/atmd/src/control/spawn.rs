//! Staged spawning lets callers build commands after cwd discovery and report
//! a new pane before typing into it. No caller environment is read here.

use atm_tmux::{PaneDirection, TmuxClient};

use super::ControlError;

/// Resolved split target, working directory and stale-caller diagnostic.
#[derive(Debug, Clone)]
pub struct SpawnContext {
    pub target_pane: String,
    pub cwd: Option<String>,
    caller_pane_missing: bool,
}

/// Prefer the explicit target, caller pane, first active pane, then `%0`.
/// Cwd lookup failures fall back to the caller-supplied directory.
pub async fn prepare_spawn(
    client: &(impl TmuxClient + ?Sized),
    cwd: Option<&str>,
    target_pane: Option<&str>,
    caller_pane: Option<&str>,
    fallback_cwd: Option<&str>,
) -> Result<SpawnContext, ControlError> {
    let panes = client
        .list_panes()
        .await
        .map_err(|source| ControlError::Tmux {
            context: "Failed to list tmux panes".to_string(),
            source,
        })?;
    let current_pane = target_pane
        .or(caller_pane)
        .or_else(|| {
            panes
                .iter()
                .find(|p| p.is_active)
                .map(|p| p.pane_id.as_str())
        })
        .unwrap_or("%0");
    let cwd = if let Some(dir) = cwd {
        Some(dir.to_string())
    } else {
        client
            .get_pane_cwd(current_pane)
            .await
            .ok()
            .flatten()
            .or_else(|| fallback_cwd.map(str::to_string))
    };
    Ok(SpawnContext {
        target_pane: current_pane.to_string(),
        cwd,
        caller_pane_missing: target_pane.is_none()
            && caller_pane.is_some()
            && !panes.iter().any(|p| p.pane_id == current_pane),
    })
}

/// Split without a command so the pane gets an interactive shell, with the
/// user's PATH and aliases. Return its id before any command is typed.
pub async fn split_spawn(
    client: &(impl TmuxClient + ?Sized),
    context: &SpawnContext,
    direction: PaneDirection,
    size: &str,
) -> Result<String, ControlError> {
    client
        .split_window(&context.target_pane, size, direction, None)
        .await
        .map_err(|source| ControlError::Tmux {
            // A stale caller deserves a hint, but a small pane does not.
            context: if context.caller_pane_missing {
                format!(
                    "Failed to split $TMUX_PANE ({}); pass -t <pane> to split another pane",
                    context.target_pane
                )
            } else {
                "Failed to split tmux pane".to_string()
            },
            source,
        })
}

/// Type an already-built command, then Enter in a separate tmux call.
pub async fn type_spawn_command(
    client: &(impl TmuxClient + ?Sized),
    pane: &str,
    command: &str,
) -> Result<(), ControlError> {
    client
        .send_keys(pane, command)
        .await
        .map_err(|source| ControlError::Tmux {
            context: "Failed to send command to new pane".to_string(),
            source,
        })?;
    client
        .send_keys(pane, "Enter")
        .await
        .map_err(|source| ControlError::Tmux {
            context: "Failed to send Enter to new pane".to_string(),
            source,
        })
}
