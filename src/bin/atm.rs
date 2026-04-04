//! ATM — Agent Tmux Manager
//!
//! CLI + TUI for managing Claude Code agents in tmux.
//!
//! # Usage
//!
//! ```text
//! atm                        # Launch TUI dashboard
//! atm --pick                 # Pick mode - exit after jumping to a session
//! atm spawn                  # Spawn a new Claude agent
//! atm kill <session-id>      # Kill an agent
//! atm interrupt <session-id> # Send SIGINT to an agent
//! atm send <session-id> <text> # Send text to agent pane
//! atm list                   # List agents (tab-separated)
//! atm status                 # One-line summary for tmux status bar
//! atm setup                  # Configure Claude Code hooks
//! atm uninstall              # Remove hooks
//! ```

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use atm_core::SessionView;
use atm_protocol::{ClientMessage, DaemonMessage};
use atm_tmux::{RealTmuxClient, TmuxClient};
use atm_tui::app::App;
use atm_tui::client::DaemonClient;
use atm_tui::daemon;
use atm_tui::error::{Result as TuiResult, TuiError};
use atm_tui::input::{ClientCommand, Event};
use atm_tui::keybinding::{InputHandler, UiAction};
use atm_tui::setup;
use atm_tui::tmux;
use atm_tui::ui;

// ============================================================================
// CLI Arguments
// ============================================================================

/// ATM — Agent Tmux Manager for Claude Code
#[derive(Parser, Debug)]
#[command(name = "atm")]
#[command(about = "Manage and monitor Claude Code agents in tmux")]
#[command(version)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Pick mode: exit after jumping to a session (fzf-style picker)
    #[arg(long, short = 'p', global = true)]
    pick: bool,

    /// Only show agents whose tmux pane belongs to this tmux session
    #[arg(long)]
    tmux_session: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Configure Claude Code hooks for atm
    Setup,
    /// Remove atm hooks from Claude Code
    Uninstall,
    /// Spawn a new Claude Code agent in a tmux pane
    Spawn {
        /// Model to use (e.g., opus, sonnet, haiku)
        #[arg(long, short = 'm')]
        model: Option<String>,
        /// Working directory for the new agent
        #[arg(long, short = 'w')]
        worktree: Option<String>,
        /// Split direction: horizontal (top/bottom) or vertical (left/right)
        #[arg(long, short = 'd', default_value = "horizontal")]
        direction: SplitDirection,
        /// Size of the new pane (e.g., "30%", "50%")
        #[arg(long, short = 's', default_value = "50%")]
        size: String,
    },
    /// Kill an agent and close its tmux pane
    Kill {
        /// Session ID (short form, e.g., "a1b2c3d4") or tmux pane ID (e.g., "%5")
        target: String,
    },
    /// Send SIGINT to interrupt an agent's current turn
    Interrupt {
        /// Session ID (short form) or tmux pane ID
        target: String,
    },
    /// Send text to an agent's tmux pane
    Send {
        /// Session ID (short form) or tmux pane ID
        target: String,
        /// Text to send
        text: String,
    },
    /// List agents for scripting
    List {
        /// Output format
        #[arg(long, short = 'f', default_value = "table")]
        format: ListFormat,
        /// Filter by status (working, idle, attention)
        #[arg(long)]
        status: Option<String>,
        /// Filter by project name
        #[arg(long)]
        project: Option<String>,
    },
    /// Show the visible content of an agent's tmux pane
    Peek {
        /// Session ID (short form) or tmux pane ID
        target: String,
        /// Only show the last N lines
        #[arg(long, short = 'n')]
        tail: Option<usize>,
        /// Extract just the active prompt (auto-detects boundaries)
        #[arg(long)]
        prompt: bool,
    },
    /// Reply to an agent's interactive prompt
    Reply {
        /// Session ID (short form) or tmux pane ID
        target: String,
        /// Option number to select (1-based), or omit to just press Enter
        #[arg(long, short = 'o')]
        option: Option<usize>,
        /// Shortcut: accept/allow (press Enter on current selection)
        #[arg(long, short = 'y', conflicts_with = "option")]
        yes: bool,
        /// Shortcut: reject/deny (press Escape)
        #[arg(long, short = 'n', conflicts_with = "option", conflicts_with = "yes")]
        no: bool,
    },
    /// One-line status summary (for tmux status bar)
    Status,
    /// Launch a tmux workspace with ATM sidebar, agent, and shell
    Workspace {
        /// Session name (default: current directory basename)
        #[arg(long)]
        name: Option<String>,
        /// Use isolated tmux server (separate from your main tmux)
        #[arg(long)]
        isolate: bool,
        /// Include an editor pane alongside the agent
        #[arg(long)]
        editor: bool,
    },
    /// Apply a layout template
    Layout {
        /// Layout name: solo, pair, squad, grid, or a custom name
        name: String,
        /// Create a new tmux session instead of a window
        #[arg(long)]
        session: Option<String>,
        /// Split the current pane in-place (default: create new window)
        #[arg(long)]
        in_place: bool,
    },
}

#[derive(Debug, Clone, ValueEnum)]
enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, ValueEnum)]
enum ListFormat {
    Table,
    Json,
    Ids,
}

// ============================================================================
// Terminal Setup / Cleanup
// ============================================================================

fn setup_terminal() -> TuiResult<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().map_err(|e| TuiError::TerminalInit(e.to_string()))?;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| TuiError::TerminalInit(e.to_string()))?;

    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(|e| TuiError::TerminalInit(e.to_string()))
}

fn cleanup_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> TuiResult<()> {
    disable_raw_mode().map_err(|e| TuiError::TerminalCleanup(e.to_string()))?;

    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .map_err(|e| TuiError::TerminalCleanup(e.to_string()))?;

    terminal
        .show_cursor()
        .map_err(|e| TuiError::TerminalCleanup(e.to_string()))?;

    Ok(())
}

// ============================================================================
// Keyboard Input Task
// ============================================================================

fn spawn_keyboard_task(
    event_tx: mpsc::UnboundedSender<Event>,
    cancel_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if cancel_token.is_cancelled() {
                debug!("Keyboard task shutting down");
                break;
            }

            let poll_result = tokio::task::spawn_blocking(|| {
                if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                    event::read().ok()
                } else {
                    None
                }
            })
            .await;

            match poll_result {
                Ok(Some(CrosstermEvent::Key(key))) => {
                    if event_tx.send(Event::Key(key)).is_err() {
                        debug!("Event channel closed, keyboard task exiting");
                        break;
                    }
                }
                Ok(Some(CrosstermEvent::Resize(width, height))) => {
                    if event_tx.send(Event::Resize(width, height)).is_err() {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => {}
                Err(e) => {
                    error!(error = %e, "Keyboard polling task panicked");
                    break;
                }
            }
        }
    })
}

// ============================================================================
// Capture Polling Task
// ============================================================================

/// Spawns a task that periodically captures the selected agent's tmux pane output.
fn spawn_capture_task(
    event_tx: mpsc::UnboundedSender<Event>,
    cancel_token: CancellationToken,
    capture_pane_rx: tokio::sync::watch::Receiver<Option<String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = RealTmuxClient::new();
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            if cancel_token.is_cancelled() {
                break;
            }

            let pane_id = capture_pane_rx.borrow().clone();
            if let Some(ref pane) = pane_id {
                if let Ok(lines) = client.capture_pane(pane).await {
                    if event_tx
                        .send(Event::CaptureUpdate {
                            pane_id: pane.clone(),
                            lines,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    })
}

// ============================================================================
// Main Event Loop
// ============================================================================

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    event_rx: &mut mpsc::UnboundedReceiver<Event>,
    command_tx: &mpsc::UnboundedSender<ClientCommand>,
    cancel_token: &CancellationToken,
    capture_pane_tx: &tokio::sync::watch::Sender<Option<String>>,
) -> Result<()> {
    let tick_rate = Duration::from_millis(100);

    // Vim keybinding handler (DFA state machine)
    let mut handler = InputHandler::new();

    // Viewport height for half-page navigation (updated each frame)
    let mut viewport_height: u16 = 0;

    loop {
        app.tick();

        // Render the UI and capture viewport height for half-page navigation
        terminal.draw(|frame| {
            let layout = ui::layout::AppLayout::new(frame.area());
            // The session list widget uses Borders::ALL, so subtract 2 to get the inner height
            viewport_height = layout.list_area.height.saturating_sub(2);
            ui::render(frame, app);
        })?;

        let event = tokio::time::timeout(tick_rate, event_rx.recv()).await;

        match event {
            Ok(Some(received_event)) => match received_event {
                Event::Key(key) => {
                    // When help is visible, intercept keys before the DFA.
                    // This is necessary because Esc maps to Quit in the DFA,
                    // but we want it to dismiss help instead.
                    if app.show_help {
                        match key.code {
                            KeyCode::Char('?') | KeyCode::Esc => {
                                app.show_help = false;
                                handler.reset();
                            }
                            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                                app.quit();
                                cancel_token.cancel();
                                break;
                            }
                            _ => {} // Swallow all other keys
                        }
                    } else if let Some(action) = handler.handle(key) {
                        match action {
                            UiAction::Quit => {
                                app.quit();
                                info!("User requested quit");
                                cancel_token.cancel();
                                break;
                            }
                            UiAction::Refresh => {
                                debug!("User requested refresh/discovery");
                                if command_tx.send(ClientCommand::Discover).is_err() {
                                    warn!("Failed to send discover command");
                                }
                            }
                            UiAction::JumpToSession => {
                                if let Some(session) = app.selected_session() {
                                    let session_id = session.id.clone();
                                    info!(session_id = %session_id, "Jump to session");
                                    if let Some(ref pane_id) = session.tmux_pane {
                                        let pane_id = pane_id.clone();
                                        match tmux::jump_to_pane(&pane_id) {
                                            Ok(()) => {
                                                info!(pane_id = %pane_id, "Jumped to pane");
                                                if app.pick_mode {
                                                    info!("Pick mode: exiting");
                                                    cancel_token.cancel();
                                                    break;
                                                }
                                            }
                                            Err(e) => {
                                                warn!(error = %e, pane_id = %pane_id, "Failed to jump");
                                            }
                                        }
                                    } else {
                                        debug!(session_id = %session_id, "No tmux pane");
                                    }
                                }
                            }
                            UiAction::MoveDown(n) => app.select_down(n),
                            UiAction::MoveUp(n) => app.select_up(n),
                            UiAction::GoToRow(index) => app.select_go_to(index),
                            UiAction::GoToLast => {
                                let last = app.tree_rows.len().saturating_sub(1);
                                app.select_go_to(last);
                            }
                            UiAction::GoToFirst => app.select_go_to(0),
                            UiAction::HalfPageDown(n) => {
                                app.select_half_page_down(n, viewport_height);
                            }
                            UiAction::HalfPageUp(n) => {
                                app.select_half_page_up(n, viewport_height);
                            }
                            UiAction::ToggleHelp => {
                                app.toggle_help();
                            }
                            UiAction::CollapseNode | UiAction::ExpandNode => {
                                app.toggle_expand();
                            }
                            UiAction::KillAgent => {
                                if let Some(session) = app.selected_session() {
                                    if let Some(ref pane_id) = session.tmux_pane {
                                        let pane_id = pane_id.clone();
                                        let client = RealTmuxClient::new();
                                        info!(pane_id = %pane_id, "Killing agent pane");
                                        tokio::spawn(async move {
                                            if let Err(e) = client.kill_pane(&pane_id).await {
                                                warn!(error = %e, "Failed to kill pane");
                                            }
                                        });
                                    }
                                }
                            }
                            UiAction::InterruptAgent => {
                                if let Some(session) = app.selected_session() {
                                    if let Some(ref pane_id) = session.tmux_pane {
                                        let pane_id = pane_id.clone();
                                        let client = RealTmuxClient::new();
                                        info!(pane_id = %pane_id, "Interrupting agent");
                                        tokio::spawn(async move {
                                            if let Err(e) = client.send_keys(&pane_id, "C-c").await
                                            {
                                                warn!(error = %e, "Failed to interrupt");
                                            }
                                        });
                                    }
                                }
                            }
                            UiAction::SpawnAgent => {
                                if tmux::is_in_tmux() {
                                    let cwd = std::env::current_dir()
                                        .ok()
                                        .map(|p| p.to_string_lossy().to_string());
                                    // Get the current ATM pane ID so we can avoid splitting it
                                    let atm_pane = std::env::var("TMUX_PANE").ok();
                                    tokio::spawn(async move {
                                        let client = RealTmuxClient::new();
                                        let panes =
                                            client.list_panes().await.ok().unwrap_or_default();
                                        // Find the largest non-ATM pane in the same session
                                        // to split into (avoids splitting the narrow sidebar)
                                        let current_session = atm_pane
                                            .as_deref()
                                            .and_then(|ap| panes.iter().find(|p| p.pane_id == ap))
                                            .map(|p| p.session_name.as_str());
                                        let target = panes
                                            .iter()
                                            .filter(|p| {
                                                // Same session
                                                current_session.map_or(true, |s| p.session_name == s)
                                                // Not the ATM pane
                                                && atm_pane.as_deref() != Some(p.pane_id.as_str())
                                            })
                                            .max_by_key(|p| (p.width as u32) * (p.height as u32))
                                            .map(|p| p.pane_id.as_str())
                                            .or(panes.iter().find(|p| p.is_active).map(|p| p.pane_id.as_str()))
                                            .unwrap_or("%0");
                                        let mut cmd = "claude".to_string();
                                        if let Some(ref dir) = cwd {
                                            cmd = format!("cd {dir} && {cmd}");
                                        }
                                        match client
                                            .split_window(target, "50%", true, Some(&cmd))
                                            .await
                                        {
                                            Ok(pane_id) => {
                                                info!(pane_id = %pane_id, "Spawned new agent")
                                            }
                                            Err(e) => warn!(error = %e, "Failed to spawn agent"),
                                        }
                                    });
                                }
                            }
                        }

                        // After any action, check if selected pane changed
                        let new_pane = app.selected_session().and_then(|s| s.tmux_pane.clone());
                        if app.capture_pane_id != new_pane {
                            app.capture_pane_id.clone_from(&new_pane);
                            app.captured_output.clear();
                            let _ = capture_pane_tx.send(new_pane);
                        }
                    }
                }
                Event::CaptureUpdate { pane_id, lines } => {
                    app.update_capture(&pane_id, lines);
                }
                Event::Resize(_width, _height) => {
                    debug!("Terminal resized");
                }
                Event::SessionUpdate(sessions) => {
                    debug!(count = sessions.len(), "Received session update");
                    app.update_sessions(sessions);
                    // Sessions may have changed — update capture target
                    let new_pane = app.selected_session().and_then(|s| s.tmux_pane.clone());
                    if app.capture_pane_id != new_pane {
                        app.capture_pane_id.clone_from(&new_pane);
                        app.captured_output.clear();
                        let _ = capture_pane_tx.send(new_pane);
                    }
                }
                Event::SessionListReplace(sessions) => {
                    debug!(count = sessions.len(), "Received full session list");
                    app.replace_sessions(sessions);
                    // Sessions replaced — update capture target
                    let new_pane = app.selected_session().and_then(|s| s.tmux_pane.clone());
                    if app.capture_pane_id != new_pane {
                        app.capture_pane_id.clone_from(&new_pane);
                        app.captured_output.clear();
                        let _ = capture_pane_tx.send(new_pane);
                    }
                }
                Event::SessionRemoved(session_id) => {
                    debug!(session_id = %session_id, "Session removed");
                    app.remove_session(&session_id);
                }
                Event::FilterUpdate(pane_ids) => {
                    app.update_filter_panes(pane_ids);
                }
                Event::DiscoveryComplete { discovered, failed } => {
                    info!(discovered, failed, "Discovery complete");
                }
                Event::DaemonDisconnected => {
                    warn!("Daemon disconnected");
                    app.mark_disconnected();
                }
            },
            Ok(None) => {
                warn!("Event channel closed");
                break;
            }
            Err(_) => {}
        }

        if app.should_quit {
            cancel_token.cancel();
            break;
        }

        if cancel_token.is_cancelled() {
            break;
        }
    }

    Ok(())
}

// ============================================================================
// Logging Setup
// ============================================================================

fn get_log_dir() -> Option<PathBuf> {
    if let Ok(xdg_state) = std::env::var("XDG_STATE_HOME") {
        return Some(PathBuf::from(xdg_state).join("atm"));
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".local/state/atm"))
}

fn create_log_file() -> Option<std::fs::File> {
    let log_dir = get_log_dir()?;

    if let Err(e) = fs::create_dir_all(&log_dir) {
        eprintln!("Warning: Failed to create log directory {log_dir:?}: {e}");
        return None;
    }

    let log_path = log_dir.join("tui.log");

    match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(file) => Some(file),
        Err(e) => {
            eprintln!("Warning: Failed to open log file {log_path:?}: {e}");
            None
        }
    }
}

// ============================================================================
// CLI Command Implementations
// ============================================================================

/// Fetches the current session list from the daemon via one-shot connection.
async fn fetch_sessions() -> Result<Vec<SessionView>> {
    let socket_path = PathBuf::from("/tmp/atm.sock");

    let stream = UnixStream::connect(&socket_path)
        .await
        .context("Failed to connect to daemon. Is atmd running?")?;

    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);

    /// Helper to send a JSON message followed by newline.
    async fn send(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        msg: &ClientMessage,
    ) -> Result<()> {
        let json = serde_json::to_string(msg).context("Failed to serialize message")?;
        writer
            .write_all(format!("{json}\n").as_bytes())
            .await
            .context("Failed to write to daemon")?;
        Ok(())
    }

    // Step 1: Connect
    send(
        &mut writer,
        &ClientMessage::connect(Some("atm-cli".to_string())),
    )
    .await?;

    // Step 2: Read Connected response, then send ListSessions
    let mut sessions = Vec::new();
    let mut line = String::new();
    let deadline = Duration::from_secs(5);

    loop {
        line.clear();
        let n = tokio::time::timeout(deadline, buf_reader.read_line(&mut line))
            .await
            .context("Timeout waiting for daemon response")?
            .context("Failed to read from daemon")?;

        if n == 0 {
            break;
        }

        if let Ok(msg) = serde_json::from_str::<DaemonMessage>(line.trim()) {
            match msg {
                DaemonMessage::Connected { .. } => {
                    // Now request the session list
                    send(&mut writer, &ClientMessage::list_sessions()).await?;
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
    let disconnect_msg = ClientMessage::disconnect();
    if let Ok(json) = serde_json::to_string(&disconnect_msg) {
        let _ = writer.write_all(format!("{json}\n").as_bytes()).await;
    }

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

async fn cmd_spawn(
    model: Option<String>,
    worktree: Option<String>,
    direction: SplitDirection,
    size: String,
) -> Result<()> {
    if !tmux::is_in_tmux() {
        bail!("atm spawn requires running inside tmux");
    }

    let client = RealTmuxClient::new();

    // Build the claude command
    let mut claude_cmd = String::from("claude");
    if let Some(ref m) = model {
        claude_cmd.push_str(&format!(" --model {m}"));
    }

    // Determine working directory
    let cwd = worktree.or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string())
    });

    if let Some(ref dir) = cwd {
        claude_cmd = format!("cd {dir} && {claude_cmd}");
    }

    // Get current pane to split from
    let panes = client
        .list_panes()
        .await
        .context("Failed to list tmux panes")?;
    let current_pane = panes
        .iter()
        .find(|p| p.is_active)
        .map(|p| p.pane_id.as_str())
        .unwrap_or("%0");

    let horizontal = matches!(direction, SplitDirection::Horizontal);

    let new_pane = client
        .split_window(current_pane, &size, horizontal, Some(&claude_cmd))
        .await
        .context("Failed to split tmux pane")?;

    println!("{new_pane}");
    Ok(())
}

async fn cmd_kill(target: String) -> Result<()> {
    daemon::ensure_daemon_running().map_err(|e| anyhow::anyhow!("Failed to start daemon: {e}"))?;
    let sessions = fetch_sessions().await?;
    let pane_id = resolve_pane_id(&sessions, &target)?;
    let client = RealTmuxClient::new();
    client
        .kill_pane(&pane_id)
        .await
        .context(format!("Failed to kill pane {pane_id}"))?;
    println!("Killed {pane_id}");
    Ok(())
}

async fn cmd_interrupt(target: String) -> Result<()> {
    daemon::ensure_daemon_running().map_err(|e| anyhow::anyhow!("Failed to start daemon: {e}"))?;
    let sessions = fetch_sessions().await?;
    let pane_id = resolve_pane_id(&sessions, &target)?;
    let client = RealTmuxClient::new();
    // C-c sends SIGINT to the foreground process in the pane
    client
        .send_keys(&pane_id, "C-c")
        .await
        .context(format!("Failed to interrupt pane {pane_id}"))?;
    println!("Interrupted {pane_id}");
    Ok(())
}

async fn cmd_send(target: String, text: String) -> Result<()> {
    daemon::ensure_daemon_running().map_err(|e| anyhow::anyhow!("Failed to start daemon: {e}"))?;
    let sessions = fetch_sessions().await?;
    let pane_id = resolve_pane_id(&sessions, &target)?;
    let client = RealTmuxClient::new();
    client
        .send_keys(&pane_id, &text)
        .await
        .context(format!("Failed to send keys to pane {pane_id}"))?;
    // Send Enter to submit the text
    client
        .send_keys(&pane_id, "Enter")
        .await
        .context(format!("Failed to send Enter to pane {pane_id}"))?;
    Ok(())
}

async fn cmd_list(
    format: ListFormat,
    status_filter: Option<String>,
    project_filter: Option<String>,
) -> Result<()> {
    daemon::ensure_daemon_running().map_err(|e| anyhow::anyhow!("Failed to start daemon: {e}"))?;
    let sessions = fetch_sessions().await?;

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
            let json =
                serde_json::to_string_pretty(&filtered).context("Failed to serialize sessions")?;
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

async fn cmd_reply(target: String, option: Option<usize>, yes: bool, no: bool) -> Result<()> {
    daemon::ensure_daemon_running().map_err(|e| anyhow::anyhow!("Failed to start daemon: {e}"))?;
    let sessions = fetch_sessions().await?;
    let pane_id = resolve_pane_id(&sessions, &target)?;
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

async fn cmd_peek(target: String, tail: Option<usize>, prompt: bool) -> Result<()> {
    daemon::ensure_daemon_running().map_err(|e| anyhow::anyhow!("Failed to start daemon: {e}"))?;
    let sessions = fetch_sessions().await?;
    let pane_id = resolve_pane_id(&sessions, &target)?;
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

async fn cmd_status() -> Result<()> {
    daemon::ensure_daemon_running().map_err(|e| anyhow::anyhow!("Failed to start daemon: {e}"))?;
    let sessions = fetch_sessions().await?;

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

// ============================================================================
// Workspace Command
// ============================================================================

async fn cmd_workspace(name: Option<String>, isolate: bool, editor: bool) -> Result<()> {
    // 1. Determine session name
    let session_name = name.unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "atm-workspace".to_string())
    });

    // 2. Create client (isolated or default)
    let socket_name = if isolate {
        Some(format!("atm-{session_name}"))
    } else {
        None
    };
    let client: RealTmuxClient = match &socket_name {
        Some(s) => RealTmuxClient::with_socket(s.clone()),
        None => RealTmuxClient::new(),
    };

    // 3. Pick layout
    let layout = if editor {
        atm_tmux::layout::preset_workspace_editor()
    } else {
        atm_tmux::layout::preset_workspace()
    };

    // 4. Apply layout (creates session + splits)
    let result = atm_tmux::layout::apply_layout(
        &client,
        &layout,
        atm_tmux::layout::LayoutTarget::NewSession(session_name.clone()),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // 5. Send commands to panes
    // Agent panes: launch claude
    if let Some(agent_panes) = result.panes.get(&atm_tmux::layout::SlotRole::Agent) {
        for pane in agent_panes {
            client
                .send_keys(pane, "claude")
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            client
                .send_keys(pane, "Enter")
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }

    // ATM panel: swap to left side, then launch filtered TUI
    if let Some(atm_panes) = result.panes.get(&atm_tmux::layout::SlotRole::AtmPanel) {
        for pane in atm_panes {
            // ATM pane was created on the right (split-window -h creates to the right).
            // Swap it to the leftmost position so the sidebar is on the left.
            let mut swap_cmd = std::process::Command::new("tmux");
            if let Some(ref socket) = socket_name {
                swap_cmd.arg("-L").arg(socket);
            }
            let _ = swap_cmd
                .args(["swap-pane", "-s", pane, "-t", &format!("{session_name}:0.0")])
                .output();

            let atm_cmd = format!("atm --tmux-session {session_name}");
            client
                .send_keys(pane, &atm_cmd)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            client
                .send_keys(pane, "Enter")
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }

    // Shell and Editor panes: leave as default shell (no action needed)

    // 6. Attach to the session
    let mut attach = std::process::Command::new("tmux");
    if let Some(ref socket) = socket_name {
        attach.arg("-L").arg(socket);
    }
    attach.arg("attach-session").arg("-t").arg(&session_name);

    // On Unix, use exec to replace the current process
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = attach.exec();
        // exec only returns on error
        bail!("Failed to attach to tmux session: {err}");
    }

    #[cfg(not(unix))]
    {
        let status = attach
            .status()
            .context("Failed to attach to tmux session")?;
        if !status.success() {
            bail!("tmux attach failed with status {status}");
        }
        Ok(())
    }
}

// ============================================================================
// Filter Task
// ============================================================================

/// Spawns a task that periodically polls tmux for pane IDs belonging to the
/// filtered session and sends FilterUpdate events.
fn spawn_filter_task(
    session_name: String,
    event_tx: mpsc::UnboundedSender<Event>,
    cancel_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = RealTmuxClient::new();
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        loop {
            interval.tick().await;
            if cancel_token.is_cancelled() {
                break;
            }

            if let Ok(panes) = client.list_panes().await {
                let pane_ids: HashSet<String> = panes
                    .iter()
                    .filter(|p| p.session_name == session_name)
                    .map(|p| p.pane_id.clone())
                    .collect();
                if event_tx.send(Event::FilterUpdate(pane_ids)).is_err() {
                    break;
                }
            }
        }
    })
}

// ============================================================================
// Main Entry Point
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Handle subcommands first (before TUI initialization)
    match args.command {
        Some(Command::Setup) => {
            return setup::setup();
        }
        Some(Command::Uninstall) => {
            return setup::uninstall();
        }
        Some(Command::Spawn {
            model,
            worktree,
            direction,
            size,
        }) => {
            return cmd_spawn(model, worktree, direction, size).await;
        }
        Some(Command::Kill { target }) => {
            return cmd_kill(target).await;
        }
        Some(Command::Interrupt { target }) => {
            return cmd_interrupt(target).await;
        }
        Some(Command::Send { target, text }) => {
            return cmd_send(target, text).await;
        }
        Some(Command::List {
            format,
            status,
            project,
        }) => {
            return cmd_list(format, status, project).await;
        }
        Some(Command::Peek {
            target,
            tail,
            prompt,
        }) => {
            return cmd_peek(target, tail, prompt).await;
        }
        Some(Command::Reply {
            target,
            option,
            yes,
            no,
        }) => {
            return cmd_reply(target, option, yes, no).await;
        }
        Some(Command::Status) => {
            return cmd_status().await;
        }
        Some(Command::Workspace {
            name,
            isolate,
            editor,
        }) => {
            return cmd_workspace(name, isolate, editor).await;
        }
        Some(Command::Layout {
            name,
            session,
            in_place,
        }) => {
            let layout =
                atm_tmux::layout::load_layout(&name, None).map_err(|e| anyhow::anyhow!("{e}"))?;

            let target = if let Some(ref session_name) = session {
                atm_tmux::layout::LayoutTarget::NewSession(session_name.clone())
            } else if in_place {
                let pane_id = std::env::var("TMUX_PANE")
                    .map_err(|_| anyhow::anyhow!("TMUX_PANE not set — are you in tmux?"))?;
                atm_tmux::layout::LayoutTarget::CurrentPane(pane_id)
            } else {
                atm_tmux::layout::LayoutTarget::NewWindow(Some(name.clone()))
            };

            let client = atm_tmux::RealTmuxClient::new();
            let result = atm_tmux::layout::apply_layout(&client, &layout, target)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            for (role, panes) in &result.panes {
                println!("{role:?}: {}", panes.join(", "));
            }
            return Ok(());
        }
        None => {}
    }

    // Initialize logging
    let log_file = create_log_file();

    if let Some(file) = log_file {
        let writer = Mutex::new(file);

        let filter =
            EnvFilter::from_default_env().add_directive("atm=info".parse().unwrap_or_else(|_| {
                tracing_subscriber::filter::Directive::from(tracing::Level::INFO)
            }));

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(writer)
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("off"))
            .init();
    }

    info!(pick_mode = args.pick, "ATM TUI starting...");

    if args.pick && !tmux::is_in_tmux() {
        bail!("--pick mode requires running inside tmux");
    }

    if let Err(e) = daemon::ensure_daemon_running() {
        bail!("Failed to ensure daemon is running: {e}");
    }

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
    let (command_tx, command_rx) = mpsc::unbounded_channel::<ClientCommand>();
    let cancel_token = CancellationToken::new();

    let mut terminal = match setup_terminal() {
        Ok(t) => t,
        Err(e) => {
            error!(error = %e, "Failed to initialize terminal");
            return Err(e.into());
        }
    };

    let mut app = if args.pick {
        App::with_pick_mode()
    } else if let Some(ref session) = args.tmux_session {
        App::with_tmux_session_filter(session.clone())
    } else {
        App::new()
    };

    let daemon_client =
        DaemonClient::with_defaults(event_tx.clone(), command_rx, cancel_token.clone());
    let daemon_handle = tokio::spawn(async move {
        daemon_client.run().await;
    });

    let keyboard_handle = spawn_keyboard_task(event_tx.clone(), cancel_token.clone());

    // Spawn filter task if --tmux-session is set
    let filter_handle = args
        .tmux_session
        .map(|session| spawn_filter_task(session, event_tx.clone(), cancel_token.clone()));

    // Create watch channel for selected pane ID and spawn capture polling task
    let (capture_pane_tx, capture_pane_rx) = tokio::sync::watch::channel(None::<String>);
    let capture_handle = spawn_capture_task(event_tx, cancel_token.clone(), capture_pane_rx);

    let result = run_event_loop(
        &mut terminal,
        &mut app,
        &mut event_rx,
        &command_tx,
        &cancel_token,
        &capture_pane_tx,
    )
    .await;

    cancel_token.cancel();

    let _ = tokio::time::timeout(Duration::from_millis(100), daemon_handle).await;
    let _ = tokio::time::timeout(Duration::from_millis(100), keyboard_handle).await;
    let _ = tokio::time::timeout(Duration::from_millis(100), capture_handle).await;
    if let Some(handle) = filter_handle {
        let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
    }

    if let Err(e) = cleanup_terminal(&mut terminal) {
        error!(error = %e, "Failed to cleanup terminal");
    }

    info!("ATM TUI stopped");

    result
}
