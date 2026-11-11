//! Coordinator tags stored in tmux's pane-level `@atm-role` option.

use std::collections::HashSet;

use atm_core::SessionView;
use atm_tmux::{TmuxClient, TmuxError};

/// Parses `pane_id<TAB>role` rows, ignoring unset or unknown roles.
#[must_use]
pub fn parse_coordinator_panes(output: &str) -> HashSet<String> {
    output
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, role)| *role == "coordinator")
        .map(|(pane, _)| pane.to_string())
        .collect()
}

/// A subagent shares its lead's pane, but never inherits its coordinator tag.
#[must_use]
pub fn is_coordinator(session: &SessionView, panes: &HashSet<String>) -> bool {
    session.parent_session_id.is_none()
        && session
            .tmux_pane
            .as_ref()
            .is_some_and(|pane| panes.contains(pane))
}

/// Reads coordinator panes on the client's tmux server.
pub async fn coordinator_panes<C: TmuxClient + ?Sized>(
    client: &C,
) -> Result<HashSet<String>, TmuxError> {
    let output = client
        .raw_command("list-panes", &["-a", "-F", "#{pane_id}\t#{@atm-role}"])
        .await?;
    Ok(parse_coordinator_panes(&output))
}

/// Sets or removes the pane option, never a window/session option.
pub async fn set_coordinator<C: TmuxClient + ?Sized>(
    client: &C,
    pane: &str,
    on: bool,
) -> Result<(), TmuxError> {
    let args: &[&str] = if on {
        &["-p", "-t", pane, "@atm-role", "coordinator"]
    } else {
        &["-pu", "-t", pane, "@atm-role"]
    };
    client.raw_command("set-option", args).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::SessionId;
    use atm_tmux::{mock::MockCall, MockTmuxClient};

    #[test]
    fn parses_only_exact_coordinator_roles() {
        assert_eq!(
            parse_coordinator_panes("%0\tcoordinator\n%1\t\n%2\timplementer\nno tab\n%3\tCoordinator\n%4\tcoordinator \n%5\tcoordinator\textra\n%0\tcoordinator\n"),
            HashSet::from(["%0".to_string()])
        );
        assert!(parse_coordinator_panes("").is_empty());
    }

    #[test]
    fn only_a_lead_on_a_tagged_pane_is_coordinator() {
        let panes = HashSet::from(["%7".to_string()]);
        let mut session = SessionView {
            tmux_pane: Some("%7".to_string()),
            ..SessionView::default()
        };
        assert!(is_coordinator(&session, &panes));
        session.parent_session_id = Some(SessionId::new("lead"));
        assert!(!is_coordinator(&session, &panes));
        session.parent_session_id = None;
        session.tmux_pane = None;
        assert!(!is_coordinator(&session, &panes));
        session.tmux_pane = Some("%8".to_string());
        assert!(!is_coordinator(&session, &panes));
    }

    #[tokio::test]
    async fn raw_commands_read_roles_and_only_write_pane_options() {
        let client = MockTmuxClient::new();
        client.set_next_raw_output("%7\tcoordinator\n%8\t\n");
        assert_eq!(
            coordinator_panes(&client).await.unwrap(),
            HashSet::from(["%7".to_string()])
        );
        set_coordinator(&client, "%7", true).await.unwrap();
        set_coordinator(&client, "%7", false).await.unwrap();
        assert_eq!(
            client.calls(),
            vec![
                MockCall::RawCommand {
                    subcommand: "list-panes".into(),
                    args: vec!["-a".into(), "-F".into(), "#{pane_id}\t#{@atm-role}".into()]
                },
                MockCall::RawCommand {
                    subcommand: "set-option".into(),
                    args: ["-p", "-t", "%7", "@atm-role", "coordinator"]
                        .map(str::to_string)
                        .to_vec()
                },
                MockCall::RawCommand {
                    subcommand: "set-option".into(),
                    args: ["-pu", "-t", "%7", "@atm-role"]
                        .map(str::to_string)
                        .to_vec()
                },
            ]
        );
    }

    #[tokio::test]
    async fn tmux_errors_are_returned() {
        let client = MockTmuxClient::new();
        client.set_next_error(atm_tmux::TmuxError::NotFound);
        assert!(matches!(
            coordinator_panes(&client).await,
            Err(atm_tmux::TmuxError::NotFound)
        ));
        for on in [true, false] {
            client.set_next_error(atm_tmux::TmuxError::Timeout {
                command: "set-option".into(),
                timeout: std::time::Duration::from_secs(5),
            });
            assert!(matches!(
                set_coordinator(&client, "%7", on).await,
                Err(atm_tmux::TmuxError::Timeout { .. })
            ));
        }
    }
}
