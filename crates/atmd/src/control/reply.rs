//! Legacy prompt navigation, returning the action instead of printing it.

use atm_core::SessionView;
use atm_tmux::TmuxClient;

use super::{resolve_pane_id, ControlError};

/// The successfully completed reply, for the caller to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyOutcome {
    Escape { pane: String },
    Enter { pane: String },
    Selected { pane: String, option: usize },
}

/// Resolve a target and answer its prompt with the legacy shortcut/navigation rules.
pub async fn reply(
    client: &(impl TmuxClient + ?Sized),
    sessions: &[SessionView],
    target: &str,
    option: Option<usize>,
    yes: bool,
    no: bool,
) -> Result<ReplyOutcome, ControlError> {
    let pane_id = resolve_pane_id(sessions, target)?;

    if no {
        // Escape dismisses/cancels the prompt
        send_key(client, &pane_id, "Escape").await?;
        return Ok(ReplyOutcome::Escape { pane: pane_id });
    }

    if yes || option.is_none() {
        // Just press Enter to accept current selection
        send_key(client, &pane_id, "Enter").await?;
        return Ok(ReplyOutcome::Enter { pane: pane_id });
    }

    // Navigate to the desired option number
    let desired = option.unwrap_or(1);

    // Capture the pane to find which option the cursor is on
    let lines = client
        .capture_pane(&pane_id)
        .await
        .map_err(|source| ControlError::Tmux {
            context: "Failed to capture pane".into(),
            source,
        })?;

    let current = find_selected_option(&lines).unwrap_or(1);

    if desired == current {
        // Already on the right option, just press Enter
        send_key(client, &pane_id, "Enter").await?;
    } else if desired > current {
        // Navigate down
        for _ in 0..(desired - current) {
            send_key(client, &pane_id, "Down").await?;
        }
        send_key(client, &pane_id, "Enter").await?;
    } else {
        // Navigate up
        for _ in 0..(current - desired) {
            send_key(client, &pane_id, "Up").await?;
        }
        send_key(client, &pane_id, "Enter").await?;
    }

    Ok(ReplyOutcome::Selected {
        pane: pane_id,
        option: desired,
    })
}

async fn send_key(
    client: &(impl TmuxClient + ?Sized),
    pane: &str,
    key: &str,
) -> Result<(), ControlError> {
    client
        .send_keys(pane, key)
        .await
        .map_err(|source| ControlError::Tmux {
            context: format!("Failed to send {key}"),
            source,
        })
}

/// Finds which numbered option currently has the ❯ cursor.
/// Returns 1-based option number, or None if no cursor found.
fn find_selected_option(lines: &[String]) -> Option<usize> {
    for line in lines {
        let trimmed = line.trim();
        // Pattern: "❯ N. ..." where N is the option number
        if let Some(rest) = trimmed.strip_prefix("❯ ") {
            // Try to parse "N." at the start
            if let Some((number, _)) = rest.split_once('.') {
                if let Ok(n) = number.trim().parse::<usize>() {
                    return Some(n);
                }
            }
        }
    }
    // No cursor found — assume option 1 (default selection)
    None
}
