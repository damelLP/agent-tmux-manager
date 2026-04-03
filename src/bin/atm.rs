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
    /// One-line status summary (for tmux status bar)
    Status,
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
// Main Event Loop
// ============================================================================

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    event_rx: &mut mpsc::UnboundedReceiver<Event>,
    command_tx: &mpsc::UnboundedSender<ClientCommand>,
    cancel_token: &CancellationToken,
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
                            KeyCode::Char('c')
                                if key.modifiers == KeyModifiers::CONTROL =>
                            {
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
                                let last = app.sessions.len().saturating_sub(1);
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
                        }
                    }
                }
                Event::Resize(_width, _height) => {
                    debug!("Terminal resized");
                }
                Event::SessionUpdate(sessions) => {
                    debug!(count = sessions.len(), "Received session update");
                    app.update_sessions(sessions);
                }
                Event::SessionListReplace(sessions) => {
                    debug!(count = sessions.len(), "Received full session list");
                    app.replace_sessions(sessions);
                }
                Event::SessionRemoved(session_id) => {
                    debug!(session_id = %session_id, "Session removed");
                    app.remove_session(&session_id);
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
    async fn send(writer: &mut tokio::net::unix::OwnedWriteHalf, msg: &ClientMessage) -> Result<()> {
        let json = serde_json::to_string(msg).context("Failed to serialize message")?;
        writer
            .write_all(format!("{json}\n").as_bytes())
            .await
            .context("Failed to write to daemon")?;
        Ok(())
    }

    // Step 1: Connect
    send(&mut writer, &ClientMessage::connect(Some("atm-cli".to_string()))).await?;

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
            bail!("Ambiguous target '{target}' matches {n} sessions: {}", ids.join(", "))
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
    let cwd = worktree
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string()));

    if let Some(ref dir) = cwd {
        claude_cmd = format!("cd {dir} && {claude_cmd}");
    }

    // Get current pane to split from
    let panes = client.list_panes().await.context("Failed to list tmux panes")?;
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
                    "attention" | "waiting" => {
                        s.status == atm_core::SessionStatus::AttentionNeeded
                    }
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
            let json = serde_json::to_string_pretty(&filtered)
                .context("Failed to serialize sessions")?;
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
        Some(Command::Status) => {
            return cmd_status().await;
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
    } else {
        App::new()
    };

    let daemon_client =
        DaemonClient::with_defaults(event_tx.clone(), command_rx, cancel_token.clone());
    let daemon_handle = tokio::spawn(async move {
        daemon_client.run().await;
    });

    let keyboard_handle = spawn_keyboard_task(event_tx, cancel_token.clone());

    let result = run_event_loop(
        &mut terminal,
        &mut app,
        &mut event_rx,
        &command_tx,
        &cancel_token,
    )
    .await;

    cancel_token.cancel();

    let _ = tokio::time::timeout(Duration::from_millis(100), daemon_handle).await;
    let _ = tokio::time::timeout(Duration::from_millis(100), keyboard_handle).await;

    if let Err(e) = cleanup_terminal(&mut terminal) {
        error!(error = %e, "Failed to cleanup terminal");
    }

    info!("ATM TUI stopped");

    result
}
