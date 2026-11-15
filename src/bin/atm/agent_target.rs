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

async fn list(root: Option<PathBuf>, budget: Duration) -> Result<ListResult> {
    let value = super::rpc::call(
        "agent.list",
        serde_json::to_value(ListParams { project_root: root })?,
        budget,
    )
    .await
    .map_err(|error| match error {
        super::rpc::RpcError::Timeout => {
            anyhow::anyhow!("agent lookup failed: timed out; nothing was sent")
        }
        other => anyhow::Error::new(other),
    })?;
    serde_json::from_value(value)
        .context("agent lookup failed: malformed answer from daemon; nothing was sent")
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

fn unique_name(rows: &[AgentListEntry], name: &str, searched: &str) -> Result<AgentId> {
    let mut matches = rows.iter().filter(|row| row.agent_id.name == name);
    let first = matches.next().with_context(|| {
        format!(
            "agent {name} was not found in {searched}; use project/name or --project-root <path>"
        )
    })?;
    if matches.next().is_some() {
        bail!("agent name {name} is ambiguous; use project/name");
    }
    Ok(first.agent_id.clone())
}

pub(super) async fn address(to: &str, root: Option<PathBuf>, budget: Duration) -> Result<Address> {
    if to == "owner" || to == "system" {
        return Ok(to.parse()?);
    }
    Ok(Address::Agent(agent(to, root, budget).await?))
}

pub(super) async fn agent(to: &str, root: Option<PathBuf>, budget: Duration) -> Result<AgentId> {
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
            let rows = list(None, budget).await?;
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
                return unique_name(&selected, to, &format!("project {project} (caller's pane)"));
            }
        }
    }
    let source = if root.is_some() {
        "--project-root"
    } else {
        "current directory"
    };
    let root = match root {
        Some(root) if root.is_absolute() => root,
        Some(root) => std::env::current_dir()?.join(root),
        None => std::env::current_dir()?,
    };
    let rows = list(Some(root.clone()), budget).await?;
    let searched = match rows.first() {
        Some(row) => format!("project {} ({source})", row.agent_id.project),
        None => format!(
            "project at {} ({source})",
            super::msg::escaped(&root.to_string_lossy(), false)
        ),
    };
    unique_name(&rows, to, &searched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::{json, Value};
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    struct RestoreEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[allow(clippy::await_holding_lock)]
    async fn stalled_command(args: &[&str], method: &str, budget: Duration, expected: &str) {
        let _lock = super::super::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("rpc.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let _restore = RestoreEnv(
            ["ATM_SOCKET", "ATM_STATE_DIR", "TMUX_PANE"]
                .into_iter()
                .map(|key| (key, std::env::var_os(key)))
                .collect(),
        );
        std::env::set_var("ATM_SOCKET", &socket);
        std::env::set_var("ATM_STATE_DIR", dir.path().join("state"));
        std::env::remove_var("TMUX_PANE");
        tokio::time::timeout(Duration::from_secs(90), async {
            let command = async {
                match super::super::Args::try_parse_from(args).unwrap().command.unwrap() {
                    super::super::Command::Agent { action } => super::super::agent::cmd_agent(action).await,
                    super::super::Command::Inbox(args) => super::super::msg::cmd_inbox(args).await,
                    super::super::Command::Msg { action } => super::super::msg::cmd_msg(action).await,
                    _ => panic!("unexpected test command"),
                }
            };
            tokio::pin!(command);
            let peer = async {
                let (mut read, mut write) = loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (read, write) = stream.into_split();
                    let mut read = BufReader::new(read);
                    let mut line = String::new();
                    if read.read_line(&mut line).await.unwrap() == 0 { continue; }
                    assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["type"], "connect");
                    break (read, write);
                };
                write.write_all(format!("{}\n", json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"test","methods":[method]})).as_bytes()).await.unwrap();
                let mut line = String::new();
                assert_ne!(read.read_line(&mut line).await.unwrap(), 0);
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], method);
                if method == "agent.list" {
                    assert_eq!(request["params"], json!({"project_root":std::env::current_dir().unwrap()}));
                }
                (read, write)
            };
            // Do not pause while startup/handshake I/O is pending: first observe the request.
            let (mut read, _write) = tokio::select! {
                peer = peer => peer,
                result = &mut command => panic!("command ended before its request: {result:?}"),
            };
            tokio::time::pause();
            tokio::time::advance(budget).await;
            let error = command.await.unwrap_err();
            tokio::time::resume();
            assert_eq!(error.to_string(), expected);
            let mut extra = String::new();
            assert_eq!(read.read_line(&mut extra).await.unwrap(), 0, "nothing follows the failed lookup");
            std::future::poll_fn(|cx| {
                assert!(listener.poll_accept(cx).is_pending(), "unexpected operation after timeout");
                std::task::Poll::Ready(())
            }).await;
        }).await.unwrap();
    }

    #[tokio::test]
    async fn clear_lookup_timeout_sends_nothing() {
        stalled_command(
            &["atm", "agent", "clear", "worker"],
            "agent.list",
            AGENT_LIST_TIMEOUT,
            "agent lookup failed: timed out; nothing was sent",
        )
        .await;
    }
    #[tokio::test]
    async fn respawn_lookup_timeout_sends_nothing() {
        stalled_command(
            &["atm", "agent", "respawn", "worker"],
            "agent.list",
            AGENT_LIST_TIMEOUT,
            "agent lookup failed: timed out; nothing was sent",
        )
        .await;
    }
    #[tokio::test]
    async fn down_lookup_timeout_sends_nothing() {
        stalled_command(
            &["atm", "agent", "down", "worker", "--kill"],
            "agent.list",
            AGENT_LIST_TIMEOUT,
            "agent lookup failed: timed out; nothing was sent",
        )
        .await;
    }
    #[tokio::test]
    async fn inbox_lookup_timeout_sends_nothing() {
        stalled_command(
            &["atm", "inbox", "--for", "worker"],
            "agent.list",
            AGENT_LIST_TIMEOUT,
            "agent lookup failed: timed out; nothing was sent",
        )
        .await;
    }
    #[tokio::test]
    async fn send_lookup_timeout_sends_nothing() {
        stalled_command(
            &["atm", "msg", "send", "--to", "worker", "--body", "hi"],
            "agent.list",
            AGENT_LIST_TIMEOUT,
            "agent lookup failed: timed out; nothing was sent",
        )
        .await;
    }
    #[tokio::test]
    async fn send_operation_timeout_keeps_outcome_unknown_warning() {
        stalled_command(&["atm", "msg", "send", "--to", "p/worker", "--body", "hi"], "msg.send", Duration::from_secs(15),
            "message request timed out; accepted work may have completed; inspect inbox before retrying").await;
    }
}
