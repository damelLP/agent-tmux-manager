//! Tmux integration for session discovery.
//!
//! Maps agent processes to the tmux panes they run in. Panes are listed
//! once per discovery scan through [`TmuxClient`], so the call shares that
//! client's deadline and `ATM_TMUX_SOCKET` handling; the per-process
//! lookup is pure.

use std::collections::HashMap;

use atm_tmux::{TmuxClient, TmuxError};

/// Lists all tmux panes as a map of shell PID to pane ID (e.g. "%5").
///
/// # Errors
/// Returns the client's [`TmuxError`] (no server, tmux not installed,
/// timeout). Discovery treats any error as "no pane info".
pub async fn list_pane_pids(tmux: &dyn TmuxClient) -> Result<HashMap<u32, String>, TmuxError> {
    let panes = tmux.list_panes().await?;
    Ok(panes
        .into_iter()
        .map(|pane| (pane.pane_pid, pane.pane_id))
        .collect())
}

/// Finds the tmux pane ID for a process.
///
/// Checks `pid`, then each of its `ancestor_pids` (nearest first), against
/// the pane shell PIDs in `pane_pids`; the first match wins.
///
/// # Returns
/// * `Some(pane_id)` - The tmux pane ID (e.g., "%5") if found
/// * `None` - If not running in tmux or pane not found
#[must_use]
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

/// A pane fixture for tests: only `pane_id` and `pane_pid` matter to
/// discovery.
#[cfg(test)]
pub(crate) fn test_pane(pane_id: &str, pane_pid: u32) -> atm_tmux::PaneInfo {
    atm_tmux::PaneInfo {
        pane_id: pane_id.to_string(),
        session_name: "s".to_string(),
        window_index: 0,
        pane_pid,
        width: 80,
        height: 24,
        is_active: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_tmux::MockTmuxClient;

    fn pane_pids(pairs: &[(u32, &str)]) -> HashMap<u32, String> {
        pairs
            .iter()
            .map(|(pid, pane_id)| (*pid, pane_id.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn list_pane_pids_maps_shell_pid_to_pane_id() {
        let mock = MockTmuxClient::new();
        mock.set_panes(vec![test_pane("%1", 100), test_pane("%2", 200)]);
        assert_eq!(
            list_pane_pids(&mock).await.ok(),
            Some(pane_pids(&[(100, "%1"), (200, "%2")]))
        );
    }

    #[tokio::test]
    async fn list_pane_pids_returns_the_tmux_error() {
        let mock = MockTmuxClient::new();
        mock.set_panes(vec![test_pane("%1", 100)]);
        mock.set_next_error(TmuxError::Timeout {
            command: "list-panes".to_string(),
            timeout: std::time::Duration::from_secs(5),
        });
        assert!(matches!(
            list_pane_pids(&mock).await,
            Err(TmuxError::Timeout { .. })
        ));
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
