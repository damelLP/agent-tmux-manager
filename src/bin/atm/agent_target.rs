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
    use std::{future::Future, task::Poll};
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

    const TEST_BUDGET: Duration = Duration::from_secs(1);
    const LOOKUP_TIMEOUT: &str = "agent lookup failed: timed out; nothing was sent";

    async fn run_command(args: &[&str], budget: Option<Duration>) -> Result<()> {
        use super::super::{agent, msg, Args, Command};
        match (Args::try_parse_from(args).unwrap().command.unwrap(), budget) {
            (Command::Agent { action }, Some(budget)) => {
                agent::cmd_agent_with_lookup_budget(action, budget).await
            }
            (Command::Inbox(args), Some(budget)) => {
                msg::cmd_inbox_with_lookup_budget(args, budget).await
            }
            (Command::Msg { action }, Some(budget)) => {
                msg::cmd_msg_with_lookup_budget(action, budget).await
            }
            (Command::Agent { action }, None) => agent::cmd_agent(action).await,
            (Command::Inbox(args), None) => msg::cmd_inbox(args).await,
            (Command::Msg { action }, None) => msg::cmd_msg(action).await,
            _ => panic!("unexpected test command"),
        }
    }

    #[allow(clippy::await_holding_lock)]
    async fn stalled_request(
        method: &str,
        params: Value,
        command: impl Future<Output = Result<()>>,
        expected: &str,
    ) {
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
        let (returned, mut returned_rx) = tokio::sync::oneshot::channel();
        let command = async {
            let result = command.await;
            eprintln!("command returned: {result:?}");
            returned.send(()).unwrap();
            result
        };
        let peer = async {
            let mut requests = Vec::new();
            let mut connections_after_request = 0;
            loop {
                let (stream, _) = tokio::select! {
                    stream = listener.accept() => stream.unwrap(),
                    returned = &mut returned_rx => {
                        returned.unwrap();
                        break;
                    }
                };
                if !requests.is_empty() {
                    connections_after_request += 1;
                }
                let (read, mut write) = stream.into_split();
                let mut read = BufReader::new(read);
                let mut line = String::new();
                if read.read_line(&mut line).await.unwrap() == 0 {
                    eprintln!("empty startup probe");
                    continue;
                }
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap()["type"],
                    "connect"
                );
                let hello = json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"test",
                    "methods":["agent.list","agent.clear","agent.respawn","agent.down","msg.inbox","msg.send"]});
                write
                    .write_all(format!("{hello}\n").as_bytes())
                    .await
                    .unwrap();
                line.clear();
                assert_ne!(read.read_line(&mut line).await.unwrap(), 0);
                let request: Value = serde_json::from_str(&line).unwrap();
                eprintln!("request: {request}");
                if requests.is_empty() {
                    assert_eq!(request["method"], method);
                    assert_eq!(request["params"], params);
                } else {
                    // Let an erroneous follow-up finish without changing its reported error.
                    let reply = json!({"type":"response","id":request["id"],"result":null});
                    write
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                }
                requests.push(request["method"].as_str().unwrap().to_owned());
                line.clear();
                assert_eq!(
                    read.read_line(&mut line).await.unwrap(),
                    0,
                    "second request on one connection: {line}"
                );
                eprintln!("request connection closed");
            }
            (requests, connections_after_request)
        };
        // Real time: both command return and connection EOF must happen before inspection.
        let (result, (requests, connections_after_request)) =
            tokio::time::timeout(Duration::from_secs(60), async {
                tokio::join!(command, peer)
            })
            .await
            .expect("command and peer must finish");
        assert_eq!(result.unwrap_err().to_string(), expected);
        assert_eq!(requests, [method], "unexpected operation after timeout");
        assert_eq!(
            connections_after_request, 0,
            "unexpected connection after request"
        );
        // Inspect the kernel queue directly, not the reactor's readiness from one poll.
        let extra = listener.into_std().unwrap().accept();
        assert!(
            matches!(&extra, Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "unexpected connection after command and EOF: {extra:?}"
        );
    }

    async fn stalled_lookup(args: &[&str]) {
        stalled_request(
            "agent.list",
            json!({"project_root": std::env::current_dir().unwrap()}),
            run_command(args, Some(TEST_BUDGET)),
            LOOKUP_TIMEOUT,
        )
        .await;
    }

    #[tokio::test]
    async fn clear_lookup_timeout_sends_nothing() {
        stalled_lookup(&["atm", "agent", "clear", "worker"]).await;
    }
    #[tokio::test]
    async fn respawn_lookup_timeout_sends_nothing() {
        stalled_lookup(&["atm", "agent", "respawn", "worker"]).await;
    }
    #[tokio::test]
    async fn down_lookup_timeout_sends_nothing() {
        stalled_lookup(&["atm", "agent", "down", "worker", "--kill"]).await;
    }
    #[tokio::test]
    async fn inbox_lookup_timeout_sends_nothing() {
        stalled_lookup(&["atm", "inbox", "--for", "worker"]).await;
    }
    #[tokio::test]
    async fn send_lookup_timeout_sends_nothing() {
        stalled_lookup(&["atm", "msg", "send", "--to", "worker", "--body", "hi"]).await;
    }

    #[tokio::test(start_paused = true)]
    #[allow(clippy::await_holding_lock)]
    async fn lookup_commands_use_twenty_second_budget() {
        let _lock = super::super::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _restore = RestoreEnv(vec![("TMUX_PANE", std::env::var_os("TMUX_PANE"))]);
        std::env::remove_var("TMUX_PANE");
        let _permit = super::super::rpc::startup_slot_for_test()
            .acquire_owned()
            .await
            .unwrap();
        // The held semaphore prevents every command from starting a thread or socket I/O.
        for args in [
            &["atm", "agent", "clear", "worker"][..],
            &["atm", "agent", "respawn", "worker"][..],
            &["atm", "agent", "down", "worker", "--kill"][..],
            &["atm", "inbox", "--for", "worker"][..],
            &["atm", "msg", "send", "--to", "worker", "--body", "hi"][..],
        ] {
            let command = run_command(args, None);
            tokio::pin!(command);
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(command.as_mut().poll(cx)))
                    .await
                    .is_pending(),
                "{args:?}"
            );
            tokio::time::advance(Duration::from_secs(19)).await;
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(command.as_mut().poll(cx)))
                    .await
                    .is_pending(),
                "lookup ended before 20 seconds: {args:?}"
            );
            tokio::time::advance(Duration::from_secs(1)).await;
            let result = std::future::poll_fn(|cx| Poll::Ready(command.as_mut().poll(cx))).await;
            let Poll::Ready(result) = result else {
                panic!("lookup still pending at 20 seconds: {args:?}");
            };
            assert_eq!(result.unwrap_err().to_string(), LOOKUP_TIMEOUT);
        }
    }

    #[tokio::test]
    async fn send_operation_timeout_keeps_outcome_unknown_warning() {
        let params =
            json!({"to":"p/worker","kind":"note","body":"hi","attachments":[],"task":null});
        let command = async {
            let _: (atmd::org::messages::SendResult, _) =
                super::super::msg::request("msg.send", &params, TEST_BUDGET).await?;
            Ok(())
        };
        stalled_request("msg.send", params.clone(), command,
            "message request timed out; accepted work may have completed; inspect inbox before retrying").await;
    }
}
