//! Owner designation of the one recorder of provisional spoken answers.
use super::msg::{output, MessageFormat};
use super::output::stdout;
use anyhow::{Context, Result};
use atm_org::AgentId;
use atmd::org::away::{RelayParams, RelayResult};
use clap::Args;
use std::time::Duration;

#[derive(Debug, Args)]
pub struct RelayArgs {
    #[arg(conflicts_with = "clear")]
    agent: Option<AgentId>,
    #[arg(long)]
    clear: bool,
    #[arg(long, value_enum, default_value = "text")]
    format: MessageFormat,
}

pub async fn cmd_relay(args: RelayArgs) -> Result<()> {
    let raw = super::rpc::call(
        "owner.relay",
        serde_json::to_value(RelayParams {
            agent: args.agent,
            clear: args.clear,
        })?,
        Duration::from_secs(27),
    )
    .await?;
    let result: RelayResult =
        serde_json::from_value(raw.clone()).context("invalid relay result from daemon")?;
    let text = format!(
        "Relay seat: {}",
        result
            .agent
            .map_or_else(|| "none".into(), |id| id.to_string())
    );
    stdout(|out| output(&raw, args.format, &text, out))
}

#[cfg(test)]
mod tests {
    use super::super::Args;
    use clap::Parser;

    // Break caught: ambiguity between show/set/clear, or accepting a partial ID.
    #[test]
    fn relay_designation_arguments_are_unambiguous() {
        for args in [
            vec!["atm", "relay"],
            vec!["atm", "relay", "project/chief"],
            vec!["atm", "relay", "--clear", "--format", "json"],
        ] {
            assert!(Args::try_parse_from(args).is_ok());
        }
        for args in [
            vec!["atm", "relay", "chief"],
            vec!["atm", "relay", "project/chief", "--clear"],
            vec!["atm", "relay", "--format", "xml"],
            vec!["atm", "relay", "--by", "owner"],
        ] {
            assert_eq!(Args::try_parse_from(args).unwrap_err().exit_code(), 2);
        }
    }
}
