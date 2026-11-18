//! Coordinator tags stored in tmux's pane-level `@atm-role` option.

use std::collections::HashSet;

use crate::{TmuxClient, TmuxError};

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
    use crate::{mock::MockCall, MockTmuxClient};
    use std::{collections::HashSet, time::Duration};

    #[test]
    fn parses_exact_roles_without_normalizing_pane_ids() {
        assert_eq!(
            parse_coordinator_panes(
                "%0\tcoordinator\n%1\t\n%2\timplementer\nno tab\n%3\tCoordinator\n%4\tcoordinator \n%5\tcoordinator\textra\n%0\tcoordinator\n\tcoordinator\n %6\tcoordinator\n%7\tcoordinator\r\n"
            ),
            HashSet::from(["%0".into(), "".into(), " %6".into(), "%7".into()])
        );
        assert!(parse_coordinator_panes("").is_empty());
    }

    #[tokio::test]
    async fn coordinator_panes_reads_and_parses_all_panes() {
        let mock = MockTmuxClient::new();
        mock.set_next_raw_output("%7\tcoordinator\n%8\t\n%9\tcoordinator\nmalformed\n");
        let client: &dyn crate::TmuxClient = &mock;
        assert_eq!(
            coordinator_panes(client).await.unwrap(),
            HashSet::from(["%7".to_string(), "%9".to_string()])
        );
        assert_eq!(
            mock.calls(),
            [MockCall::RawCommand {
                subcommand: "list-panes".into(),
                args: ["-a", "-F", "#{pane_id}\t#{@atm-role}"]
                    .map(str::to_string)
                    .to_vec(),
            }]
        );
    }

    #[tokio::test]
    async fn coordinator_panes_returns_an_empty_successful_read() {
        let client = MockTmuxClient::new();
        client.set_next_raw_output("%7\t\n%8\timplementer\n");
        assert!(coordinator_panes(&client).await.unwrap().is_empty());
        client.set_next_raw_output("");
        assert!(coordinator_panes(&client).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn set_coordinator_sets_and_unsets_only_the_given_pane() {
        let mock = MockTmuxClient::new();
        let client: &dyn crate::TmuxClient = &mock;
        set_coordinator(client, "%7", true).await.unwrap();
        set_coordinator(client, "%8", false).await.unwrap();
        assert_eq!(
            mock.calls(),
            [
                MockCall::RawCommand {
                    subcommand: "set-option".into(),
                    args: ["-p", "-t", "%7", "@atm-role", "coordinator"]
                        .map(str::to_string)
                        .to_vec(),
                },
                MockCall::RawCommand {
                    subcommand: "set-option".into(),
                    args: ["-pu", "-t", "%8", "@atm-role"]
                        .map(str::to_string)
                        .to_vec(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn role_io_returns_command_errors_without_retry() {
        for on in [None, Some(true), Some(false)] {
            let client = MockTmuxClient::new();
            client.set_next_error(crate::TmuxError::CommandFailed {
                command: "role-command".into(),
                stderr: "no such pane".into(),
            });
            let result = match on {
                None => coordinator_panes(&client).await.map(|_| ()),
                Some(on) => set_coordinator(&client, "%7", on).await,
            };
            assert!(matches!(
                result,
                Err(crate::TmuxError::CommandFailed { command, stderr })
                    if command == "role-command" && stderr == "no such pane"
            ));
            assert_eq!(client.call_count(), 1);
        }
    }

    #[tokio::test]
    async fn role_io_returns_timeout_errors_unchanged() {
        for on in [None, Some(true), Some(false)] {
            let client = MockTmuxClient::new();
            client.set_next_error(crate::TmuxError::Timeout {
                command: "role-command".into(),
                timeout: Duration::from_secs(5),
            });
            let result = match on {
                None => coordinator_panes(&client).await.map(|_| ()),
                Some(on) => set_coordinator(&client, "%7", on).await,
            };
            assert!(matches!(
                result,
                Err(crate::TmuxError::Timeout { command, timeout })
                    if command == "role-command" && timeout == Duration::from_secs(5)
            ));
            assert_eq!(client.call_count(), 1);
        }
    }
}
