//! Legacy prompt navigation, returning the action instead of printing it.

#[cfg(test)]
#[path = "reply_tests.rs"]
mod tests;

#[cfg(test)]
use atm_core::SessionView;
use atm_tmux::TmuxClient;

#[cfg(test)]
use super::resolve_pane_id;
use super::ControlError;

/// The successfully completed reply, for the caller to render.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReplyOutcome {
    Escape { pane: String },
    Enter { pane: String },
    Selected { pane: String, option: usize },
}

/// Resolve a target and answer its prompt with the legacy shortcut/navigation rules.
#[cfg(test)]
pub async fn reply(
    client: &(impl TmuxClient + ?Sized),
    sessions: &[SessionView],
    target: &str,
    option: Option<usize>,
    yes: bool,
    no: bool,
) -> Result<ReplyOutcome, ControlError> {
    let pane = resolve_pane_id(sessions, target)?;
    let prepared = prepare_reply(client, pane, option, yes, no).await?;
    finish_reply(client, prepared, || Ok(())).await
}

/// Captured navigation, with no input sent yet. Keep the existing outcome as
/// the single description of what the caller renders.
pub(super) struct PreparedReply {
    outcome: ReplyOutcome,
    navigation: Option<(&'static str, usize)>,
}

impl PreparedReply {
    pub(super) fn submit_moves(&self) -> Option<bool> {
        (!matches!(self.outcome, ReplyOutcome::Escape { .. }))
            .then(|| self.navigation.is_some_and(|(_, count)| count != 0))
    }
}

pub(super) async fn prepare_reply(
    client: &(impl TmuxClient + ?Sized),
    pane_id: String,
    option: Option<usize>,
    yes: bool,
    no: bool,
) -> Result<PreparedReply, ControlError> {
    if no {
        return Ok(PreparedReply {
            outcome: ReplyOutcome::Escape { pane: pane_id },
            navigation: None,
        });
    }

    if yes || option.is_none() {
        return Ok(PreparedReply {
            outcome: ReplyOutcome::Enter { pane: pane_id },
            navigation: None,
        });
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

    Ok(PreparedReply {
        outcome: ReplyOutcome::Selected {
            pane: pane_id,
            option: desired,
        },
        navigation: Some((
            if desired > current { "Down" } else { "Up" },
            desired.abs_diff(current),
        )),
    })
}

/// Admission is checked for every key, even when tmux futures are immediately
/// ready. The legacy caller supplies a no-op; the daemon supplies its deadline.
pub(super) async fn finish_reply<E: From<ControlError>>(
    client: &(impl TmuxClient + ?Sized),
    prepared: PreparedReply,
    mut before_key: impl FnMut() -> Result<(), E>,
) -> Result<ReplyOutcome, E> {
    let pane = match &prepared.outcome {
        ReplyOutcome::Escape { pane }
        | ReplyOutcome::Enter { pane }
        | ReplyOutcome::Selected { pane, .. } => pane,
    };
    if let Some((key, count)) = prepared.navigation {
        for _ in 0..count {
            before_key()?;
            send_key(client, pane, key).await?;
        }
    }
    before_key()?;
    let key = if matches!(prepared.outcome, ReplyOutcome::Escape { .. }) {
        "Escape"
    } else {
        "Enter"
    };
    send_key(client, pane, key).await?;
    Ok(prepared.outcome)
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
