//! Thin, one-shot named-agent RPC commands. No local tmux fallback or retry.

use super::output::stdout;
use atmd::org::agent_handover::{HandoverParams, HandoverResult};
use atmd::org::agents::*;
use clap::{Subcommand, ValueEnum};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use std::{
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AgentFormat {
    Text,
    Json,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AgentListFormat {
    Table,
    Json,
}

const BARE_NAME_HELP: &str =
    "Bare seat name. The project comes from --project-root or the current directory.";

#[derive(Debug, Subcommand)]
pub enum AgentAction {
    /// Start a named agent from its role in a new tmux pane
    Up {
        #[arg(value_parser = parse_name, help = BARE_NAME_HELP)]
        name: String,
        #[arg(long)]
        project_root: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "text")]
        format: AgentFormat,
    },
    /// Register an existing pane as a named agent without restarting its program
    Adopt {
        #[arg(value_parser = parse_name, help = BARE_NAME_HELP)]
        name: String,
        #[arg(value_parser = parse_pane)]
        pane: String,
        #[arg(long)]
        project_root: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "text")]
        format: AgentFormat,
    },
    /// Clear an agent's current context in its existing pane
    Clear {
        #[arg(value_parser = super::agent_target::parse_agent)]
        agent: String,
        #[arg(long)]
        project_root: Option<PathBuf>,
        /// Clear timeout in milliseconds (5000..=120000).
        #[arg(long, default_value_t = 30_000, value_parser = clap::value_parser!(u64).range(5_000..=120_000))]
        timeout_ms: u64,
        #[arg(long, value_enum, default_value = "text")]
        format: AgentFormat,
    },
    /// Ask an agent to save its state to its memory file, then clear its context
    Handover {
        #[arg(value_parser = super::agent_target::parse_agent)]
        agent: String,
        #[arg(long)]
        project_root: Option<PathBuf>,
        #[arg(long, default_value_t = 600_000, value_parser = clap::value_parser!(u64).range(30_000..=1_800_000))]
        timeout_ms: u64,
        #[arg(long, value_enum, default_value = "text")]
        format: AgentFormat,
    },
    /// Restart an agent in a new pane, closing its old pane if present
    Respawn {
        #[arg(value_parser = super::agent_target::parse_agent)]
        agent: String,
        #[arg(long)]
        project_root: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "text")]
        format: AgentFormat,
    },
    /// Take an agent down, leaving its pane open unless --kill is passed
    Down {
        #[arg(value_parser = super::agent_target::parse_agent)]
        agent: String,
        #[arg(long)]
        project_root: Option<PathBuf>,
        /// Close the pane and its program; without it the seat is released and the program keeps running
        #[arg(long)]
        kill: bool,
        #[arg(long, value_enum, default_value = "text")]
        format: AgentFormat,
    },
    /// List named agents and their status, optionally filtered by project
    List {
        #[arg(long)]
        project_root: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "table")]
        format: AgentListFormat,
    },
}

fn parse_name(name: &str) -> Result<String, String> {
    "validation"
        .parse()
        .and_then(|project| AgentId::new(project, name))
        .map(|agent| agent.name)
        .map_err(|error| format!("{error}; use the bare seat name; the project comes from --project-root or the current directory"))
}

fn parse_pane(pane: &str) -> Result<String, String> {
    atmd::control::reservation::PaneId::parse(pane)
        .map(|pane| pane.to_string())
        .map_err(|error| error.to_string())
}

fn absolute_root(root: Option<PathBuf>) -> io::Result<PathBuf> {
    match root {
        Some(root) if root.is_absolute() => Ok(root),
        Some(root) => Ok(std::env::current_dir()?.join(root)),
        None => std::env::current_dir(),
    }
}

async fn request<P: Serialize, R: DeserializeOwned + Serialize>(
    method: &str,
    params: P,
    seconds: u64,
) -> anyhow::Result<Value> {
    let value = super::rpc::call(
        method,
        serde_json::to_value(params)?,
        Duration::from_secs(seconds),
    )
    .await?;
    // Decode the method-owned result, while allowing additive wire fields.
    let result: R = serde_json::from_value(value)?;
    Ok(serde_json::to_value(result)?)
}

fn cell(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "-".into(),
        Some(Value::String(text)) => text
            .replace('\\', "\\\\")
            .replace('\t', "\\t")
            .replace('\n', "\\n")
            .replace('\r', "\\r"),
        Some(value) => value.to_string(),
    }
}

fn row(value: &Value, columns: &[&str]) -> String {
    columns
        .iter()
        .map(|key| {
            if *key == "cost_usd" {
                value
                    .get(key)
                    .and_then(Value::as_f64)
                    .map(|cost| format!("{cost:.6}"))
                    .unwrap_or_else(|| "-".into())
            } else {
                cell(value.get(key))
            }
        })
        .collect::<Vec<_>>()
        .join("\t")
}

pub async fn cmd_agent(action: AgentAction) -> anyhow::Result<()> {
    cmd_agent_with_lookup_budget(action, super::agent_target::AGENT_LIST_TIMEOUT).await
}

pub(super) async fn cmd_agent_with_lookup_budget(
    action: AgentAction,
    lookup_budget: Duration,
) -> anyhow::Result<()> {
    let show_release_note = matches!(&action, AgentAction::Down { kill: false, .. });
    let (value, json, columns, list) = match action {
        AgentAction::Up {
            name,
            project_root,
            format,
        } => (
            request::<_, UpResult>(
                "agent.up",
                UpParams {
                    name,
                    project_root: absolute_root(project_root)?,
                    caller_server: None,
                },
                85,
            )
            .await?,
            matches!(format, AgentFormat::Json),
            &["agent_id", "pane", "state"][..],
            false,
        ),
        AgentAction::Adopt {
            name,
            pane,
            project_root,
            format,
        } => (
            request::<_, AdoptResult>(
                "agent.adopt",
                AdoptParams {
                    name,
                    pane,
                    project_root: absolute_root(project_root)?,
                    caller_server: None,
                },
                55,
            )
            .await?,
            matches!(format, AgentFormat::Json),
            &["agent_id", "state"][..],
            false,
        ),
        AgentAction::Clear {
            agent,
            project_root,
            timeout_ms,
            format,
        } => (
            request::<_, ClearResult>(
                "agent.clear",
                ClearParams {
                    agent: super::agent_target::agent(&agent, project_root, lookup_budget).await?,
                    timeout_ms,
                },
                timeout_ms.div_ceil(1000) + 25,
            )
            .await?,
            matches!(format, AgentFormat::Json),
            &["agent_id", "session_id"][..],
            false,
        ),
        AgentAction::Handover {
            agent,
            project_root,
            timeout_ms,
            format,
        } => (
            request::<_, HandoverResult>(
                "agent.handover",
                HandoverParams {
                    agent: super::agent_target::agent(&agent, project_root, lookup_budget).await?,
                    timeout_ms,
                },
                // The save, then the clear's 120 s, then room for the answer.
                timeout_ms.div_ceil(1000) + 160,
            )
            .await?,
            matches!(format, AgentFormat::Json),
            &["agent_id", "state", "session_id", "message_id"][..],
            false,
        ),
        AgentAction::Respawn {
            agent,
            project_root,
            format,
        } => (
            request::<_, RespawnResult>(
                "agent.respawn",
                RespawnParams {
                    agent: super::agent_target::agent(&agent, project_root, lookup_budget).await?,
                    caller_server: None,
                },
                85,
            )
            .await?,
            matches!(format, AgentFormat::Json),
            &["agent_id", "pane"][..],
            false,
        ),
        AgentAction::Down {
            agent,
            project_root,
            kill,
            format,
        } => (
            request::<_, DownResult>(
                "agent.down",
                DownParams {
                    agent: super::agent_target::agent(&agent, project_root, lookup_budget).await?,
                    kill,
                },
                55,
            )
            .await?,
            matches!(format, AgentFormat::Json),
            &["agent_id"][..],
            false,
        ),
        AgentAction::List {
            project_root,
            format,
        } => (
            request::<_, ListResult>(
                "agent.list",
                ListParams {
                    project_root: project_root
                        .map(|root| absolute_root(Some(root)))
                        .transpose()?,
                },
                20,
            )
            .await?,
            matches!(format, AgentListFormat::Json),
            &[
                "agent_id",
                "state",
                "activity",
                "pane",
                "session_id",
                "harness",
                "model",
                "cost_usd",
                "reason",
            ][..],
            true,
        ),
    };
    stdout(|out| {
        if json {
            writeln!(out, "{value}")?;
        } else if list {
            writeln!(
                out,
                "AGENT\tSTATE\tACTIVITY\tPANE\tSESSION\tHARNESS\tMODEL\tCOST\tREASON"
            )?;
            if let Some(rows) = value.as_array() {
                for value in rows {
                    writeln!(out, "{}", row(value, columns))?;
                }
            }
        } else {
            writeln!(out, "{}", row(&value, columns))?;
            if show_release_note {
                writeln!(
                    out,
                    "Without --kill, any existing pane and its program keep running; --kill closes them instead."
                )?;
            }
        }
        Ok(())
    })
}
