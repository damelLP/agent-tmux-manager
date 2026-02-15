//! ATM TUI - htop-style monitoring for Claude Code agents
//!
//! This binary provides a terminal user interface for monitoring
//! Claude Code sessions across tmux windows.
//!
//! # Usage
//!
//! ```text
//! atm          # Normal mode - stay running
//! atm --pick   # Pick mode - exit after jumping to a session
//! atm setup    # Configure Claude Code hooks
//! atm uninstall # Remove hooks
//! ```

use std::fs::{self, OpenOptions};
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, Event as CrosstermEvent},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

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

/// ATM TUI - htop-style monitoring for Claude Code agents
#[derive(Parser, Debug)]
#[command(name = "atm")]
#[command(about = "Monitor Claude Code sessions in real-time")]
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
                    if let Some(action) = handler.handle(key) {
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
