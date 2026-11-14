//! Thin, one-shot named-agent RPC commands. No local tmux fallback or retry.

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
#[derive(Debug, Subcommand)]
pub enum AgentAction {
    Up {
        name: String,
        #[arg(long)]
        project_root: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "text")]
        format: AgentFormat,
    },
    Adopt {
        name: String,
        pane: String,
        #[arg(long)]
        project_root: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "text")]
        format: AgentFormat,
    },
    Clear {
        agent: AgentId,
        #[arg(long, default_value_t = 30_000, value_parser = clap::value_parser!(u64).range(1..=120_000))]
        timeout_ms: u64,
        #[arg(long, value_enum, default_value = "text")]
        format: AgentFormat,
    },
    Respawn {
        agent: AgentId,
        #[arg(long, value_enum, default_value = "text")]
        format: AgentFormat,
    },
    Down {
        agent: AgentId,
        #[arg(long)]
        kill: bool,
        #[arg(long, value_enum, default_value = "text")]
        format: AgentFormat,
    },
    List {
        #[arg(long)]
        project_root: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "table")]
        format: AgentListFormat,
    },
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
            timeout_ms,
            format,
        } => (
            request::<_, ClearResult>(
                "agent.clear",
                ClearParams { agent, timeout_ms },
                timeout_ms.div_ceil(1000) + 25,
            )
            .await?,
            matches!(format, AgentFormat::Json),
            &["agent_id", "session_id"][..],
            false,
        ),
        AgentAction::Respawn { agent, format } => (
            request::<_, RespawnResult>("agent.respawn", RespawnParams { agent }, 85).await?,
            matches!(format, AgentFormat::Json),
            &["agent_id", "pane"][..],
            false,
        ),
        AgentAction::Down {
            agent,
            kill,
            format,
        } => (
            request::<_, DownResult>("agent.down", DownParams { agent, kill }, 55).await?,
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
    let mut out = io::stdout().lock();
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
    }
    Ok(())
}
