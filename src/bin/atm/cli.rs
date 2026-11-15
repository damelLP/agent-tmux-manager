//! CLI commands that act on a session: kill, interrupt, send, role, list, reply, peek and status.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use atm_core::SessionView;
use atm_protocol::{ClientMessage, DaemonMessage};
use atm_tmux::RealTmuxClient;
use atm_tui::{daemon, role};
use atmd::control::{resolve_pane_id, ReplyOutcome};

use super::{spawn::non_empty_env, ListFormat};

// ============================================================================
// CLI Command Implementations
// ============================================================================

/// Fetches the current session list from the daemon via one-shot connection.
async fn fetch_sessions() -> Result<Vec<SessionView>> {
    let socket_path = atm_tui::client::resolve_socket_path();

    // One deadline bounds every step of this one-shot exchange.
    let deadline = Duration::from_secs(5);

    let stream = tokio::time::timeout(deadline, UnixStream::connect(&socket_path))
        .await
        .context("Timeout connecting to daemon")?
        .context("Failed to connect to daemon. Is atmd running?")?;

    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);

    /// Helper to send a JSON message followed by newline.
    async fn send(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        msg: &ClientMessage,
        deadline: Duration,
    ) -> Result<()> {
        let json = serde_json::to_string(msg).context("Failed to serialize message")?;
        tokio::time::timeout(deadline, writer.write_all(format!("{json}\n").as_bytes()))
            .await
            .context("Timeout writing to daemon")?
            .context("Failed to write to daemon")?;
        Ok(())
    }

    // Step 1: Connect
    send(
        &mut writer,
        &ClientMessage::connect(Some("atm-cli".to_string())),
        deadline,
    )
    .await?;

    // Step 2: Read Connected response, then send ListSessions
    let sessions;
    let mut line = String::new();

    loop {
        line.clear();
        let n = tokio::time::timeout(deadline, buf_reader.read_line(&mut line))
            .await
            .context("Timeout waiting for daemon response")?
            .context("Failed to read from daemon")?;

        if n == 0 {
            bail!("Daemon closed the connection before sending the session list");
        }

        if let Ok(msg) = serde_json::from_str::<DaemonMessage>(line.trim()) {
            match msg {
                DaemonMessage::Connected { .. } => {
                    // Now request the session list
                    send(&mut writer, &ClientMessage::list_sessions(), deadline).await?;
                    continue;
                }
                DaemonMessage::SessionList {
                    sessions: sess_list,
                } => {
                    sessions = sess_list;
                    break;
                }
                DaemonMessage::Rejected { reason, .. } => {
                    bail!("Daemon rejected connection: {reason}");
                }
                DaemonMessage::Error { message, .. } => {
                    bail!("Daemon error: {message}");
                }
                _ => continue,
            }
        }
    }

    // Send disconnect
    let _ = send(&mut writer, &ClientMessage::disconnect(), deadline).await;

    Ok(sessions)
}

/// Starts the daemon if needed, then fetches its session list.
async fn daemon_sessions() -> Result<Vec<SessionView>> {
    daemon::ensure_daemon_running().map_err(|e| anyhow::anyhow!("Failed to start daemon: {e}"))?;
    fetch_sessions().await
}

/// Fetches the daemon's session list, then resolves a CLI target in it with
/// `resolve_pane_id` (which returns a `%` pane ID as is).
async fn pane_for_target(target: &str) -> Result<String> {
    Ok(resolve_pane_id(&daemon_sessions().await?, target)?)
}

pub(super) async fn cmd_role(target: Option<String>, on: bool) -> Result<()> {
    // Only an explicit session target may start or contact the daemon.
    let pane = match target {
        Some(target) if !target.starts_with('%') => pane_for_target(&target).await?,
        Some(target) => target,
        None => non_empty_env("TMUX_PANE")
            .context("TMUX_PANE is not set; pass -t <target> to choose a pane")?,
    };
    role::set_coordinator(&RealTmuxClient::new(), &pane, on).await?;
    Ok(())
}

/// One daemon request that names a target and answers with the pane it acted on.
async fn pane_request(method: &str, target: String) -> Result<String> {
    let result = super::rpc::call(
        method,
        serde_json::json!({"target": target}),
        Duration::from_secs(40),
    )
    .await?;
    result
        .get("pane")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("Invalid {method} response: missing pane"))
}

pub(super) async fn cmd_kill(target: String) -> Result<()> {
    let pane_id = pane_request("session.kill", target).await?;
    println!("Killed {pane_id}");
    Ok(())
}

pub(super) async fn cmd_interrupt(target: String) -> Result<()> {
    let pane_id = pane_request("session.interrupt", target).await?;
    println!("Interrupted {pane_id}");
    Ok(())
}

pub(super) async fn cmd_send(target: String, text: String) -> Result<()> {
    let result = super::rpc::call(
        "session.send",
        serde_json::json!({"target": target, "text": text}),
        Duration::from_secs(40),
    )
    .await?;
    let pane = result
        .get("pane")
        .and_then(serde_json::Value::as_str)
        .context("Invalid session.send response: missing pane")?;
    println!("Sent to {pane} by tmux paste");
    Ok(())
}

/// CLI-only metadata: the daemon's SessionView and wire format stay unchanged.
#[derive(serde::Serialize)]
struct ListedSession<'a> {
    #[serde(flatten)]
    session: &'a SessionView,
    role: Option<&'static str>,
}

pub(super) async fn cmd_list(
    format: ListFormat,
    status_filter: Option<String>,
    project_filter: Option<String>,
) -> Result<()> {
    let sessions = daemon_sessions().await?;

    // Apply filters
    let filtered: Vec<&SessionView> = sessions
        .iter()
        .filter(|s| {
            if let Some(ref status) = status_filter {
                let matches = match status.to_lowercase().as_str() {
                    "working" | "active" => s.status == atm_core::SessionStatus::Working,
                    "idle" => s.status == atm_core::SessionStatus::Idle,
                    "attention" | "waiting" => s.status == atm_core::SessionStatus::AttentionNeeded,
                    _ => true,
                };
                if !matches {
                    return false;
                }
            }
            if let Some(ref project) = project_filter {
                if let Some(ref root) = s.project_root {
                    if !root.contains(project.as_str()) {
                        return false;
                    }
                } else {
                    return false;
                }
            }
            true
        })
        .collect();

    match format {
        ListFormat::Table => {
            for s in &filtered {
                let status = s.status_label.as_str();
                let ctx = format!("{:.0}%", s.context_percentage);
                let model = &s.model;
                let project = s
                    .project_root
                    .as_deref()
                    .and_then(|p| p.rsplit('/').find(|s| !s.is_empty()))
                    .unwrap_or("-");
                let branch = s.worktree_branch.as_deref().unwrap_or("");
                let project_display = if branch.is_empty() {
                    project.to_string()
                } else {
                    format!("{project}/{branch}")
                };
                let pane = s.tmux_pane.as_deref().unwrap_or("-");
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    s.id_short, status, ctx, model, project_display, pane
                );
            }
        }
        ListFormat::Json => {
            let panes = role::coordinator_panes(&RealTmuxClient::new())
                .await
                .unwrap_or_default();
            let rows: Vec<_> = filtered
                .iter()
                .map(|session| {
                    // Rows can share an id (one per live process); prefer the same pane.
                    let parent = session.parent_session_id.as_ref().and_then(|id| {
                        sessions
                            .iter()
                            .filter(|s| s.id == *id)
                            .min_by_key(|s| s.tmux_pane != session.tmux_pane)
                    });
                    ListedSession {
                        session,
                        role: role::is_coordinator(session, parent, &panes)
                            .then_some("coordinator"),
                    }
                })
                .collect();
            let json =
                serde_json::to_string_pretty(&rows).context("Failed to serialize sessions")?;
            println!("{json}");
        }
        ListFormat::Ids => {
            for s in &filtered {
                println!("{}", s.id_short);
            }
        }
    }

    Ok(())
}

pub(super) async fn cmd_reply(
    target: String,
    option: Option<usize>,
    yes: bool,
    no: bool,
) -> Result<()> {
    let result = super::rpc::call(
        "session.reply",
        serde_json::json!({"target":target,"option":option,"yes":yes,"no":no}),
        Duration::from_secs(40),
    )
    .await?;
    let outcome: ReplyOutcome =
        serde_json::from_value(result).context("Invalid session.reply response")?;
    match outcome {
        ReplyOutcome::Escape { pane } => println!("Sent Escape to {pane}"),
        ReplyOutcome::Enter { pane } => println!("Sent Enter to {pane}"),
        ReplyOutcome::Selected { pane, option } => println!("Selected option {option} on {pane}"),
    }
    Ok(())
}

pub(super) async fn cmd_peek(target: String, tail: Option<usize>, prompt: bool) -> Result<()> {
    let result = super::rpc::call(
        "session.capture",
        serde_json::json!({"target":target,"tail":tail,"prompt":prompt}),
        Duration::from_secs(40),
    )
    .await?;
    let lines = result
        .get("lines")
        .context("Invalid session.capture response: missing lines")?;
    let output: Vec<String> =
        serde_json::from_value(lines.clone()).context("Invalid session.capture response: lines")?;
    for line in output {
        println!("{line}");
    }
    Ok(())
}

pub(super) async fn cmd_status() -> Result<()> {
    let sessions = daemon_sessions().await?;

    let active = sessions
        .iter()
        .filter(|s| {
            matches!(
                s.status,
                atm_core::SessionStatus::Working | atm_core::SessionStatus::Idle
            )
        })
        .count();

    let attention = sessions
        .iter()
        .filter(|s| s.status == atm_core::SessionStatus::AttentionNeeded)
        .count();

    let total_cost: f64 = sessions.iter().map(|s| s.cost_usd).sum();

    let mut parts = vec![format!("{active}\u{2191}")]; // ↑
    if attention > 0 {
        parts.push(format!("{attention}!"));
    }
    parts.push(format!("${total_cost:.2}"));
    use atmd::org::messages::{Address, InboxParams};
    let unread = super::msg::inbox_request(InboxParams {
        recipient: Some(Address::Owner),
        unread_only: false,
        count_only: true,
        id: None,
        after: None,
        limit: 100,
    })
    .await
    .and_then(|result| {
        if !result.messages.is_empty() || result.next_after.is_some() {
            bail!("invalid aggregate result from daemon");
        }
        Ok(result.unread_count)
    });
    match unread {
        Ok(0) => {}
        Ok(count) => parts.push(format!("{count} unread")),
        Err(error) => {
            parts.push("? unread".into());
            eprintln!("unread count unavailable: {error}");
        }
    }
    match super::decide::pending_count().await {
        Ok((count, away)) => {
            if count > 0 {
                parts.push(format!("{count} decisions"));
            }
            // So that a forgotten `atm back` shows in the status line.
            if away {
                parts.push("away".into());
            }
        }
        Err(error) => {
            parts.push("? decisions".into());
            eprintln!("decision count unavailable: {error}");
        }
    }
    use std::io::Write;
    writeln!(std::io::stdout().lock(), "{}", parts.join(" "))?;

    Ok(())
}

#[cfg(test)]
mod list_tests {
    use super::*;

    #[test]
    fn flattened_rows_keep_every_existing_field_byte() {
        let populated = SessionView {
            project_root: Some("/a\"b".into()),
            worktree_path: Some("/work".into()),
            worktree_branch: Some("main".into()),
            parent_session_id: Some(atm_core::SessionId::new("lead")),
            child_session_ids: vec![atm_core::SessionId::new("child")],
            first_prompt: Some("hello\nworld".into()),
            ..SessionView::default()
        };
        for session in [SessionView::default(), populated] {
            let before = serde_json::to_string(&session).unwrap();
            for role in [None, Some("coordinator")] {
                let after = serde_json::to_string(&ListedSession {
                    session: &session,
                    role,
                })
                .unwrap();
                let suffix = format!(",\"role\":{}}}", serde_json::to_string(&role).unwrap());
                assert_eq!(
                    format!("{}}}", after.strip_suffix(&suffix).unwrap()),
                    before
                );
            }
        }
    }
}
