//! Pane-local coordinator metadata, independent of harness/reservation state.

use std::collections::HashSet;

use atm_tmux::TmuxClient;

use super::ControlError;

pub(super) async fn coordinator_panes(
    client: &dyn TmuxClient,
) -> Result<HashSet<String>, ControlError> {
    atm_tmux::role::coordinator_panes(client)
        .await
        .map_err(|source| ControlError::Tmux {
            context: "Failed to read pane roles".into(),
            source,
        })
}

pub(super) async fn set_coordinator(
    client: &dyn TmuxClient,
    pane: &str,
    on: bool,
) -> Result<(), ControlError> {
    atm_tmux::role::set_coordinator(client, pane, on)
        .await
        .map_err(|source| ControlError::Tmux {
            context: format!("Failed to set role on {pane}"),
            source,
        })
}
