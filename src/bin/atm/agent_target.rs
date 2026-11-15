//! Project-scoped CLI agent lookup; daemon methods still receive full IDs.

use anyhow::{bail, Context, Result};
use atmd::{
    control::reservation::PaneId,
    org::{
        agents::{AgentId, AgentListEntry, AgentStatus, ListParams, ListResult},
        messages::Address,
    },
};
use std::{path::PathBuf, time::Duration};

// 10 s agent.list router + 10 s startup/transport margin = 20 s.
pub(super) const AGENT_LIST_TIMEOUT: Duration = Duration::from_secs(20);

async fn list(root: Option<PathBuf>) -> Result<ListResult> {
    super::msg::request(
        "agent.list",
        ListParams { project_root: root },
        AGENT_LIST_TIMEOUT,
    )
    .await
    .map(|(result, _)| result)
}

pub(super) fn parse_agent(text: &str) -> Result<String, String> {
    let validated = if text.contains('/') {
        text.parse::<AgentId>()
    } else {
        "validation"
            .parse()
            .and_then(|project| AgentId::new(project, text))
    };
    validated
        .map(|_| text.to_owned())
        .map_err(|error| error.to_string())
}

pub(super) fn parse_address(text: &str) -> Result<String, String> {
    if text == "owner" || text == "system" {
        return Ok(text.to_owned());
    }
    parse_agent(text)
}

fn unique_name(rows: &[AgentListEntry], name: &str) -> Result<AgentId> {
    let mut matches = rows.iter().filter(|row| row.agent_id.name == name);
    let first = matches
        .next()
        .with_context(|| format!("agent {name} was not found in the selected project; use project/name or --project-root <path>"))?;
    if matches.next().is_some() {
        bail!("agent name {name} is ambiguous; use project/name");
    }
    Ok(first.agent_id.clone())
}

pub(super) async fn address(to: &str, root: Option<PathBuf>) -> Result<Address> {
    if to == "owner" || to == "system" {
        return Ok(to.parse()?);
    }
    Ok(Address::Agent(agent(to, root).await?))
}

pub(super) async fn agent(to: &str, root: Option<PathBuf>) -> Result<AgentId> {
    if to.contains('/') {
        return Ok(to.parse()?);
    }
    // Reuse the domain's name validation rather than maintaining a second grammar.
    AgentId::new("validation".parse()?, to)?;
    let pane = super::spawn::non_empty_env("TMUX_PANE")
        .map(PaneId::parse)
        .transpose()?;
    if root.is_none() {
        if let Some(pane) = pane {
            let rows = list(None).await?;
            let mut bindings = rows.iter().filter(|row| {
                row.state != AgentStatus::Down && row.pane.as_deref() == Some(pane.as_str())
            });
            if let Some(caller) = bindings.next() {
                if bindings.next().is_some() {
                    bail!("caller pane has ambiguous agent bindings");
                }
                let project = caller.agent_id.project.clone();
                let selected: Vec<_> = rows
                    .into_iter()
                    .filter(|row| row.agent_id.project == project)
                    .collect();
                return unique_name(&selected, to);
            }
        }
    }
    let root = match root {
        Some(root) if root.is_absolute() => root,
        Some(root) => std::env::current_dir()?.join(root),
        None => std::env::current_dir()?,
    };
    unique_name(&list(Some(root)).await?, to)
}
