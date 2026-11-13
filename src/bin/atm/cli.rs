//! CLI commands that act on a session: kill, interrupt, send, role, list, reply, peek and status.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use atm_core::SessionView;
use atm_protocol::{ClientMessage, DaemonMessage};
use atm_tmux::{RealTmuxClient, TmuxClient};
use atm_tui::{daemon, role};

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

/// Resolves a target (session ID prefix or pane ID) to a tmux pane ID.
fn resolve_pane_id(sessions: &[SessionView], target: &str) -> Result<String> {
    // If it starts with %, it's already a pane ID
    if target.starts_with('%') {
        return Ok(target.to_string());
    }

    // Search by session ID prefix
    let matches: Vec<&SessionView> = sessions
        .iter()
        .filter(|s| s.id.as_str().starts_with(target) || s.id_short.starts_with(target))
        .collect();

    match matches.len() {
        0 => bail!("No session matching '{target}'"),
        1 => match &matches[0].tmux_pane {
            Some(pane) => Ok(pane.clone()),
            None => bail!("Session {} has no tmux pane", matches[0].id_short),
        },
        n => {
            let ids: Vec<&str> = matches.iter().map(|s| s.id_short.as_str()).collect();
            bail!(
                "Ambiguous target '{target}' matches {n} sessions: {}",
                ids.join(", ")
            )
        }
    }
}

/// Starts the daemon if needed, then fetches its session list.
async fn daemon_sessions() -> Result<Vec<SessionView>> {
    daemon::ensure_daemon_running().map_err(|e| anyhow::anyhow!("Failed to start daemon: {e}"))?;
    fetch_sessions().await
}

/// Fetches the daemon's session list, then resolves a CLI target in it with
/// `resolve_pane_id` (which returns a `%` pane ID as is).
async fn pane_for_target(target: &str) -> Result<String> {
    resolve_pane_id(&daemon_sessions().await?, target)
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

pub(super) async fn cmd_kill(target: String) -> Result<()> {
    let pane_id = pane_for_target(&target).await?;
    let client = RealTmuxClient::new();
    client
        .kill_pane(&pane_id)
        .await
        .context(format!("Failed to kill pane {pane_id}"))?;
    println!("Killed {pane_id}");
    Ok(())
}

pub(super) async fn cmd_interrupt(target: String) -> Result<()> {
    let pane_id = pane_for_target(&target).await?;
    let client = RealTmuxClient::new();
    // C-c sends SIGINT to the foreground process in the pane
    client
        .send_keys(&pane_id, "C-c")
        .await
        .context(format!("Failed to interrupt pane {pane_id}"))?;
    println!("Interrupted {pane_id}");
    Ok(())
}

pub(super) async fn cmd_send(target: String, text: String) -> Result<()> {
    let sessions = daemon_sessions().await?;
    let pane_id = resolve_pane_id(&sessions, &target)?;
    let client = RealTmuxClient::new();
    let pane_state = client
        .raw_command(
            "display-message",
            &[
                "-p",
                "-t",
                &pane_id,
                "#{pane_in_mode} #{bracket_paste_flag}",
            ],
        )
        .await
        .context(format!("Failed to read the state of pane {pane_id}"))?;
    if let Some(reason) = send_refusal(&sessions, &target, &pane_id, &pane_state, &text) {
        bail!("Refusing to send to {pane_id}: {reason}");
    }
    client
        .paste_text(&pane_id, &text)
        .await
        .context(format!("Failed to paste into pane {pane_id}"))?;
    // Send Enter to submit the text, on its own call: tmux reads an
    // argument ending in `;` as a command separator.
    client
        .send_keys(&pane_id, "Enter")
        .await
        .context(format!("Failed to send Enter to pane {pane_id}"))?;
    println!("Sent to {pane_id} by tmux paste");
    Ok(())
}

/// Why `atm send` must not paste `text` into `pane` for `target`, if it
/// must not (#116). `pane_state` is `#{pane_in_mode} #{bracket_paste_flag}`;
/// tmux before 3.7 has no `bracket_paste_flag` and leaves it empty, which
/// counts as unknown.
fn send_refusal(
    sessions: &[SessionView],
    target: &str,
    pane: &str,
    pane_state: &str,
    text: &str,
) -> Option<&'static str> {
    let mut fields = pane_state.split_whitespace();
    let in_mode = fields.next().is_some_and(|modes| modes != "0");
    let bracketed_paste = fields.next();
    let in_pane = || {
        sessions
            .iter()
            .filter(|s| s.tmux_pane.as_deref() == Some(pane))
    };
    let named = sessions
        .iter()
        .find(|s| s.id.as_str().starts_with(target) || s.id_short.starts_with(target));
    // An in-process child, such as a Claude background subagent, carries its
    // lead's pane. A `%N` target names the pane itself.
    if !target.starts_with('%')
        && named
            .and_then(|s| s.parent_session_id.as_ref())
            .is_some_and(|parent| in_pane().any(|s| &s.id == parent))
    {
        return Some("the session is a subagent in its lead's pane; the text would reach the lead");
    }
    // A dialog (permission, question, plan) takes the paste, and Enter picks
    // its highlighted option. A subagent's dialog shows in its lead's pane.
    // A dialog denied with Esc sends no event, so the status can outlast it.
    if in_pane().any(|s| s.status == atm_core::SessionStatus::AttentionNeeded) {
        return Some(
            "the session is waiting on a dialog that Enter would answer; use `atm reply`, \
             or, if the dialog is gone, type in the pane",
        );
    }
    if in_mode {
        return Some("the pane is in copy mode (or another mode), so Enter would go to tmux");
    }
    if bracketed_paste == Some("0") && text.contains(['\n', '\r']) {
        return Some("the program didn't ask for bracketed paste, so each line would submit alone");
    }
    None
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
                    let parent = session
                        .parent_session_id
                        .as_ref()
                        .and_then(|id| sessions.iter().find(|s| s.id == *id));
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
    let pane_id = pane_for_target(&target).await?;
    let client = RealTmuxClient::new();

    if no {
        // Escape dismisses/cancels the prompt
        client
            .send_keys(&pane_id, "Escape")
            .await
            .context("Failed to send Escape")?;
        println!("Sent Escape to {pane_id}");
        return Ok(());
    }

    if yes || option.is_none() {
        // Just press Enter to accept current selection
        client
            .send_keys(&pane_id, "Enter")
            .await
            .context("Failed to send Enter")?;
        println!("Sent Enter to {pane_id}");
        return Ok(());
    }

    // Navigate to the desired option number
    let desired = option.unwrap_or(1);

    // Capture the pane to find which option the cursor is on
    let lines = client
        .capture_pane(&pane_id)
        .await
        .context("Failed to capture pane")?;

    let current = find_selected_option(&lines).unwrap_or(1);

    if desired == current {
        // Already on the right option, just press Enter
        client
            .send_keys(&pane_id, "Enter")
            .await
            .context("Failed to send Enter")?;
    } else if desired > current {
        // Navigate down
        for _ in 0..(desired - current) {
            client
                .send_keys(&pane_id, "Down")
                .await
                .context("Failed to send Down")?;
        }
        client
            .send_keys(&pane_id, "Enter")
            .await
            .context("Failed to send Enter")?;
    } else {
        // Navigate up
        for _ in 0..(current - desired) {
            client
                .send_keys(&pane_id, "Up")
                .await
                .context("Failed to send Up")?;
        }
        client
            .send_keys(&pane_id, "Enter")
            .await
            .context("Failed to send Enter")?;
    }

    println!("Selected option {desired} on {pane_id}");
    Ok(())
}

/// Finds which numbered option currently has the ❯ cursor.
/// Returns 1-based option number, or None if no cursor found.
fn find_selected_option(lines: &[String]) -> Option<usize> {
    for line in lines {
        let trimmed = line.trim();
        // Pattern: "❯ N. ..." where N is the option number
        if let Some(rest) = trimmed.strip_prefix("❯ ") {
            // Try to parse "N." at the start
            if let Some(dot_pos) = rest.find('.') {
                if let Ok(n) = rest[..dot_pos].trim().parse::<usize>() {
                    return Some(n);
                }
            }
        }
    }
    // No cursor found — assume option 1 (default selection)
    None
}

pub(super) async fn cmd_peek(target: String, tail: Option<usize>, prompt: bool) -> Result<()> {
    let pane_id = pane_for_target(&target).await?;
    let client = RealTmuxClient::new();
    let lines = client
        .capture_pane(&pane_id)
        .await
        .context(format!("Failed to capture pane {pane_id}"))?;

    let output: &[String] = if prompt {
        extract_prompt(&lines)
    } else if let Some(n) = tail {
        let start = lines.len().saturating_sub(n);
        &lines[start..]
    } else {
        &lines
    };

    for line in output {
        println!("{line}");
    }
    Ok(())
}

/// Extracts the active prompt from captured pane content.
///
/// Scans backwards from the bottom looking for Claude Code prompt patterns:
/// - Footer line: "Enter to select" / "Esc to cancel" / "(Y)es/(N)o"
/// - Numbered options: "1. ...", "2. ..."
/// - Separator lines (box drawing: ─── or ═══)
/// - Question/title lines
/// - Title blocks (☐/□/■)
///
/// Falls back to the last 15 lines if no prompt pattern is detected.
fn extract_prompt(lines: &[String]) -> &[String] {
    if lines.is_empty() {
        return lines;
    }

    // Find the footer line (bottom of prompt)
    let footer_idx = lines.iter().rposition(|l| {
        let trimmed = l.trim();
        trimmed.contains("Enter to select")
            || trimmed.contains("Esc to cancel")
            || trimmed.contains("(Y)es")
            || trimmed.contains("(y/n)")
            || trimmed.contains("to navigate")
    });

    let end = footer_idx.map_or(lines.len(), |i| i + 1);

    // Scan backwards from footer to find the top of the prompt block.
    // Strategy: keep going as long as lines look like prompt content.
    // Stop when we hit a line that's clearly not part of the prompt
    // (e.g., regular output, the ● response marker, the ❯ input line).
    let mut start = end.saturating_sub(1);

    for i in (0..end.saturating_sub(1)).rev() {
        let trimmed = lines[i].trim();

        // Part of prompt: numbered options, indented descriptions, cursor
        if trimmed.starts_with("1.")
            || trimmed.starts_with("2.")
            || trimmed.starts_with("3.")
            || trimmed.starts_with("4.")
            || trimmed.starts_with("5.")
            || trimmed.starts_with("6.")
            || trimmed.starts_with("7.")
            || trimmed.starts_with("8.")
            || trimmed.starts_with("9.")
        {
            start = i;
            continue;
        }

        // Cursor indicator on option
        if trimmed.starts_with("❯ ") && trimmed.len() > 2 {
            // "❯ 1. Yes" is a prompt option; bare "❯ " is the input line
            let after_cursor = trimmed.get(4..).unwrap_or(""); // skip "❯ "
            if after_cursor.starts_with("1.")
                || after_cursor.starts_with("2.")
                || after_cursor.starts_with("3.")
                || after_cursor.starts_with("4.")
                || after_cursor.starts_with("5.")
            {
                start = i;
                continue;
            }
        }

        // Indented description line (part of an option)
        if lines[i].starts_with("    ") && !trimmed.is_empty() {
            start = i;
            continue;
        }

        // Separator line (─── or ═══ etc.)
        if is_separator_line(trimmed) {
            start = i;
            continue;
        }

        // Empty line (spacing within prompt)
        if trimmed.is_empty() {
            start = i;
            continue;
        }

        // Question line (contains ?)
        if trimmed.contains('?') {
            start = i;
            continue;
        }

        // Title block (☐/□/■)
        if trimmed.starts_with('☐') || trimmed.starts_with('□') || trimmed.starts_with('■') {
            start = i;
            break; // Title is the top of the prompt
        }

        // If we've already found prompt content (start < end-1),
        // a non-matching line means we've gone past the prompt
        if start < end.saturating_sub(1) {
            break;
        }
    }

    // If we didn't find any prompt markers, fall back to last 15 lines
    if start == end.saturating_sub(1) && footer_idx.is_none() {
        let fallback_start = lines.len().saturating_sub(15);
        return &lines[fallback_start..];
    }

    // Trim leading blank/separator lines from the extracted prompt
    while start < end {
        let trimmed = lines.get(start).map(|l| l.trim()).unwrap_or("");
        if trimmed.is_empty() || is_separator_line(trimmed) {
            start += 1;
        } else {
            break;
        }
    }

    &lines[start..end]
}

/// Returns true if a line is a box-drawing separator (────, ════, etc.)
fn is_separator_line(trimmed: &str) -> bool {
    if trimmed.len() < 3 {
        return false;
    }
    let first_char = trimmed.chars().next().unwrap_or(' ');
    matches!(first_char, '─' | '━' | '═' | '┄' | '┈')
        && trimmed
            .chars()
            .all(|c| matches!(c, '─' | '━' | '═' | '┄' | '┈' | ' '))
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
    println!("{}", parts.join(" "));

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

#[cfg(test)]
mod prompt_tests {
    use super::extract_prompt;

    #[test]
    fn test_extract_numbered_prompt() {
        let lines: Vec<String> = vec![
            "Some previous output",
            "",
            "Do you like pineapple on pizza?",
            "",
            "❯ 1. Yes",
            "    Pineapple belongs on pizza",
            "  2. No",
            "    Pineapple does not belong on pizza",
            "  3. Type something.",
            "",
            "  4. Chat about this",
            "",
            "Enter to select · ↑/↓ to navigate · Esc to cancel",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let prompt = extract_prompt(&lines);
        let text = prompt
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("pineapple on pizza"),
            "should contain the question"
        );
        assert!(text.contains("1. Yes"), "should contain option 1");
        assert!(text.contains("4. Chat"), "should contain option 4");
        assert!(text.contains("Enter to select"), "should contain footer");
        assert!(
            !text.contains("previous output"),
            "should not contain earlier output"
        );
    }

    #[test]
    fn test_extract_yn_prompt() {
        let lines: Vec<String> = vec![
            "lots of code output here",
            "more code",
            "",
            "Allow Bash: git status? (Y)es/(N)o",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let prompt = extract_prompt(&lines);
        assert!(prompt.iter().any(|l| l.contains("(Y)es")));
    }

    #[test]
    fn test_fallback_when_no_prompt() {
        let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();

        let prompt = extract_prompt(&lines);
        assert_eq!(prompt.len(), 15);
    }

    #[test]
    fn test_empty_input() {
        let lines: Vec<String> = vec![];
        let prompt = extract_prompt(&lines);
        assert!(prompt.is_empty());
    }
}

#[cfg(test)]
mod send_tests {
    use super::send_refusal;
    use atm_core::{SessionId, SessionStatus, SessionView};

    fn row(id: &str, pane: Option<&str>, parent: Option<&str>) -> SessionView {
        SessionView {
            id: SessionId::new(id),
            id_short: id.chars().take(8).collect(),
            tmux_pane: pane.map(str::to_string),
            parent_session_id: parent.map(SessionId::new),
            ..SessionView::default()
        }
    }

    fn needs_input(mut session: SessionView) -> SessionView {
        session.status = SessionStatus::AttentionNeeded;
        session
    }

    #[test]
    fn a_command_or_message_to_an_idle_lead_is_sent() {
        let sessions = [row("lead-aaaa", Some("%5"), None)];
        for text in ["/clear", "!ls", "run the tests;"] {
            assert_eq!(
                send_refusal(&sessions, "lead", "%5", "0 1\n", text),
                None,
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_pane_in_copy_mode_is_refused() {
        let sessions = [row("lead-aaaa", Some("%5"), None)];
        let reason = send_refusal(&sessions, "%5", "%5", "1 1\n", "hello");
        assert!(reason.is_some_and(|r| r.contains("copy mode")));
    }

    #[test]
    fn multi_line_text_is_refused_only_when_the_flag_is_explicitly_off() {
        let sessions = [row("lead-aaaa", Some("%5"), None)];
        let two_lines = "line one\nline two";
        assert!(send_refusal(&sessions, "%5", "%5", "0 0\n", two_lines)
            .is_some_and(|r| r.contains("bracketed paste")));
        assert!(send_refusal(&sessions, "%5", "%5", "0 0\n", "a\rb").is_some());
        assert_eq!(
            send_refusal(&sessions, "%5", "%5", "0 1\n", two_lines),
            None
        );
        // tmux before 3.7 has no `bracket_paste_flag` and prints it empty.
        assert_eq!(send_refusal(&sessions, "%5", "%5", "0 \n", two_lines), None);
        assert_eq!(
            send_refusal(&sessions, "%5", "%5", "0 0\n", "one line"),
            None
        );
    }

    #[test]
    fn a_subagent_in_its_leads_pane_is_refused() {
        let sessions = [
            row("lead-aaaa", Some("%5"), None),
            row("child-bbbb", Some("%5"), Some("lead-aaaa")),
        ];
        let reason = send_refusal(&sessions, "child", "%5", "0 1\n", "hi");
        assert!(reason.is_some_and(|r| r.contains("subagent") && r.contains("lead")));
        // A pane target means the pane itself, which the lead reads.
        assert_eq!(send_refusal(&sessions, "%5", "%5", "0 1\n", "hi"), None);
    }

    #[test]
    fn a_teammate_with_its_own_pane_is_sent() {
        let sessions = [
            row("lead-aaaa", Some("%5"), None),
            row("mate-cccc", Some("%7"), Some("lead-aaaa")),
        ];
        assert_eq!(send_refusal(&sessions, "mate", "%7", "0 1\n", "hi"), None);
    }

    #[test]
    fn a_dialog_in_the_target_pane_is_refused() {
        let reason = |sessions: &[SessionView], target| {
            send_refusal(sessions, target, "%5", "0 1\n", "/clear")
        };
        let lead_at_dialog = [needs_input(row("lead-aaaa", Some("%5"), None))];
        // The status can outlive a dialog denied with Esc, so the reason
        // names the way out too.
        assert!(reason(&lead_at_dialog, "lead")
            .is_some_and(|r| r.contains("atm reply") && r.contains("type in the pane")));
        assert!(reason(&lead_at_dialog, "%5").is_some());
        // A background subagent's dialog shows in its lead's pane.
        let child_at_dialog = [
            row("lead-aaaa", Some("%5"), None),
            needs_input(row("child-bbbb", Some("%5"), Some("lead-aaaa"))),
        ];
        assert!(reason(&child_at_dialog, "lead").is_some());
        // A dialog in another pane doesn't matter.
        let other_pane = [
            row("lead-aaaa", Some("%5"), None),
            needs_input(row("else-dddd", Some("%9"), None)),
        ];
        assert_eq!(reason(&other_pane, "lead"), None);
    }
}
