//! Exact session-name pane membership for the TUI's session filter.

use std::collections::HashSet;

use atm_tmux::{TmuxClient, TmuxError};

/// List all panes and retain only exact session-name matches.
/// An empty successful set is distinct from a failed tmux read.
pub async fn session_panes(
    client: &(impl TmuxClient + ?Sized),
    session_name: &str,
) -> Result<HashSet<String>, TmuxError> {
    let panes = client.list_panes().await?;
    Ok(panes
        .iter()
        .filter(|p| p.session_name == session_name)
        .map(|p| p.pane_id.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::session_panes;
    use std::collections::HashSet;

    use atm_tmux::{mock::MockCall, MockTmuxClient, PaneInfo, TmuxError};

    fn pane(pane_id: &str, session_name: &str, window_index: u32) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.into(),
            session_name: session_name.into(),
            window_index,
            pane_pid: 1234,
            width: 80,
            height: 24,
            is_active: true,
        }
    }

    #[tokio::test]
    async fn membership_uses_exact_session_name_across_windows() {
        let client = MockTmuxClient::new();
        client.set_panes(vec![
            pane("%0", "work", 0),
            pane("%1", "work-extra", 0),
            pane("%2", "work", 1),
            pane("%3", "Work", 0),
        ]);

        assert_eq!(
            session_panes(&client, "work").await.unwrap(),
            HashSet::from(["%0".into(), "%2".into()])
        );
        assert_eq!(client.calls(), vec![MockCall::ListPanes]);
        assert_eq!(
            session_panes(&client, "work-extra").await.unwrap(),
            HashSet::from(["%1".into()])
        );
        assert_eq!(
            session_panes(&client, "Work").await.unwrap(),
            HashSet::from(["%3".into()])
        );
    }

    #[tokio::test]
    async fn successful_list_without_matching_session_returns_empty_set() {
        let client = MockTmuxClient::new();
        client.set_panes(vec![pane("%0", "work-extra", 0)]);

        assert_eq!(
            session_panes(&client, "work").await.unwrap(),
            HashSet::new()
        );
    }

    #[tokio::test]
    async fn successful_empty_list_returns_empty_set() {
        let client = MockTmuxClient::new();

        assert_eq!(
            session_panes(&client, "work").await.unwrap(),
            HashSet::new()
        );
    }

    #[tokio::test]
    async fn failed_list_remains_error_not_empty_success() {
        let client = MockTmuxClient::new();
        client.set_panes(vec![pane("%0", "work", 0)]);
        client.set_next_error(TmuxError::CommandFailed {
            command: "list-panes".into(),
            stderr: "server unavailable".into(),
        });

        let result = session_panes(&client, "work").await;

        assert!(matches!(
            result,
            Err(TmuxError::CommandFailed { command, stderr })
                if command == "list-panes" && stderr == "server unavailable"
        ));
        assert_eq!(client.calls(), vec![MockCall::ListPanes]);
    }
}
