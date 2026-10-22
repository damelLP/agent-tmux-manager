//! Tmux integration for session discovery.
//!
//! Maps agent processes to the tmux panes they run in. Panes are listed
//! once per discovery scan through [`TmuxClient`], so the call shares that
//! client's deadline and `ATM_TMUX_SOCKET` handling; the per-process
//! lookup is pure.

use std::collections::HashMap;

use atm_tmux::TmuxClient;
use tracing::debug;

/// Lists all tmux panes as a map of shell PID to pane ID (e.g. "%5").
///
/// A tmux failure (no server, tmux not installed, timeout) yields an empty
/// map: discovery still registers sessions, just without a pane.
pub async fn list_pane_pids(tmux: &dyn TmuxClient) -> HashMap<u32, String> {
    match tmux.list_panes().await {
        Ok(panes) => panes
            .into_iter()
            .map(|pane| (pane.pane_pid, pane.pane_id))
            .collect(),
        Err(e) => {
            debug!(error = %e, "No tmux panes listed");
            HashMap::new()
        }
    }
}

/// Finds the tmux pane ID for a process.
///
/// Checks `pid`, then each of its `ancestor_pids` (nearest first), against
/// the pane shell PIDs in `pane_pids`; the first match wins.
///
/// # Returns
/// * `Some(pane_id)` - The tmux pane ID (e.g., "%5") if found
/// * `None` - If not running in tmux or pane not found
pub fn find_pane_for_pid(
    pid: u32,
    ancestor_pids: &[u32],
    pane_pids: &HashMap<u32, String>,
) -> Option<String> {
    std::iter::once(&pid)
        .chain(ancestor_pids)
        .find_map(|p| pane_pids.get(p))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_tmux::{MockTmuxClient, PaneInfo, TmuxError};

    fn pane(pane_id: &str, pane_pid: u32) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.to_string(),
            session_name: "s".to_string(),
            window_index: 0,
            pane_pid,
            width: 80,
            height: 24,
            is_active: false,
        }
    }

    fn pane_pids(pairs: &[(u32, &str)]) -> HashMap<u32, String> {
        pairs
            .iter()
            .map(|(pid, pane_id)| (*pid, pane_id.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn list_pane_pids_maps_shell_pid_to_pane_id() {
        let mock = MockTmuxClient::new();
        mock.set_panes(vec![pane("%1", 100), pane("%2", 200)]);
        assert_eq!(
            list_pane_pids(&mock).await,
            pane_pids(&[(100, "%1"), (200, "%2")])
        );
    }

    #[tokio::test]
    async fn list_pane_pids_is_empty_when_tmux_fails() {
        let mock = MockTmuxClient::new();
        mock.set_panes(vec![pane("%1", 100)]);
        mock.set_next_error(TmuxError::Timeout {
            command: "list-panes".to_string(),
            timeout: std::time::Duration::from_secs(5),
        });
        assert!(list_pane_pids(&mock).await.is_empty());
    }

    #[test]
    fn find_pane_matches_the_process_itself() {
        let panes = pane_pids(&[(100, "%1")]);
        assert_eq!(find_pane_for_pid(100, &[50], &panes).as_deref(), Some("%1"));
    }

    #[test]
    fn find_pane_prefers_the_nearest_ancestor() {
        let panes = pane_pids(&[(50, "%outer"), (70, "%inner")]);
        assert_eq!(
            find_pane_for_pid(100, &[70, 50], &panes).as_deref(),
            Some("%inner")
        );
    }

    #[test]
    fn find_pane_is_none_outside_tmux() {
        let panes = pane_pids(&[(999, "%1")]);
        assert_eq!(find_pane_for_pid(100, &[70, 50], &panes), None);
        assert_eq!(find_pane_for_pid(100, &[70, 50], &HashMap::new()), None);
    }
}
