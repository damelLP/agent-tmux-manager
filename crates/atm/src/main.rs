//! ATM TUI - htop-style monitoring for Claude Code agents
//!
//! This binary provides a terminal user interface for monitoring
//! Claude Code sessions across tmux windows.
//!
//! # Architecture
//!
//! The TUI uses an event-driven architecture with three main components:
//!
//! 1. **Keyboard Task**: Polls for keyboard input and sends events to the main loop
//! 2. **Daemon Client Task**: Maintains connection to the daemon and forwards session updates
//! 3. **Main Event Loop**: Processes events, updates state, and renders the UI
//!
//! All tasks respect a shared `CancellationToken` for graceful shutdown.
//!
//! # Usage
//!
//! ```text
//! atm          # Normal mode - stay running
//! atm --pick   # Pick mode - exit after jumping to a session
//! ```

use std::fs::{self, OpenOptions};
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

mod app;
mod client;
mod daemon;
mod error;
mod input;
mod keybinding;
mod setup;
mod tmux;
mod ui;

use crate::app::App;
use crate::client::DaemonClient;
use crate::error::{Result as TuiResult, TuiError};
use crate::input::{ClientCommand, Event};
use crate::keybinding::{InputHandler, UiAction};

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

/// Initializes the terminal for TUI rendering.
///
/// Sets up:
/// - Raw mode (disable line buffering, echo)
/// - Alternate screen buffer (preserves original terminal content)
/// - Crossterm backend for ratatui
///
/// # Returns
///
/// * `Ok(Terminal)` - Configured terminal ready for rendering
/// * `Err(TuiError)` - If terminal initialization fails
fn setup_terminal() -> TuiResult<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().map_err(|e| TuiError::TerminalInit(e.to_string()))?;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| TuiError::TerminalInit(e.to_string()))?;

    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(|e| TuiError::TerminalInit(e.to_string()))
}

/// Restores the terminal to its original state.
///
/// This should always be called before exiting, even on error.
/// Restores:
/// - Normal (cooked) mode
/// - Original screen buffer
/// - Visible cursor
///
/// # Arguments
///
/// * `terminal` - The terminal to cleanup
///
/// # Returns
///
/// * `Ok(())` - Cleanup successful
/// * `Err(TuiError)` - If cleanup fails (terminal may be in bad state)
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

/// Spawns a task that polls for keyboard input and sends events to the channel.
///
/// This runs in a blocking context using `spawn_blocking` since crossterm's
/// event polling is synchronous. The task checks the cancellation token
/// periodically to allow graceful shutdown.
///
/// # Arguments
///
/// * `event_tx` - Channel to send keyboard events
/// * `cancel_token` - Token to signal shutdown
///
/// # Returns
///
/// * `JoinHandle<()>` - Handle to await task completion on shutdown
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

            // Use spawn_blocking for the synchronous crossterm poll
            let poll_result = tokio::task::spawn_blocking(|| {
                // Poll with a short timeout to allow cancellation checks
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
                        // Channel closed, receiver dropped
                        debug!("Event channel closed, keyboard task exiting");
                        break;
                    }
                }
                Ok(Some(CrosstermEvent::Resize(width, height))) => {
                    if event_tx.send(Event::Resize(width, height)).is_err() {
                        break;
                    }
                }
                Ok(Some(_)) => {
                    // Other events (Mouse, Paste, etc.) - ignore
                }
                Ok(None) => {
                    // No event, continue polling
                }
                Err(e) => {
                    // JoinError from spawn_blocking - task panicked
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

/// Runs the main TUI event loop.
///
/// This function:
/// 1. Renders the UI
/// 2. Waits for events (with timeout for tick)
/// 3. Processes events and updates state
/// 4. Repeats until quit is requested
///
/// # Arguments
///
/// * `terminal` - The terminal to render to
/// * `app` - The application state
/// * `event_rx` - Channel to receive events from
/// * `command_tx` - Channel to send commands to the daemon client
/// * `cancel_token` - Token to signal shutdown
///
/// # Returns
///
/// * `Ok(())` - Loop exited cleanly (user quit)
/// * `Err` - An error occurred during the loop
async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    event_rx: &mut mpsc::UnboundedReceiver<Event>,
    command_tx: &mpsc::UnboundedSender<ClientCommand>,
    cancel_token: &CancellationToken,
) -> Result<()> {
    // Tick interval for periodic UI refresh (e.g., updating timestamps)
    let tick_rate = Duration::from_millis(100);

    // Vim keybinding handler (DFA state machine)
    let mut handler = InputHandler::new();

    // Viewport height for half-page navigation (updated each frame)
    let mut viewport_height: u16 = 0;

    loop {
        // Tick animation state (for blinking icons)
        app.tick();

        // Render the UI and capture viewport height for half-page navigation
        terminal.draw(|frame| {
            let layout = ui::layout::AppLayout::new(frame.area());
            // The session list widget uses Borders::ALL, so subtract 2 to get the inner height
            viewport_height = layout.list_area.height.saturating_sub(2);
            ui::render(frame, app);
        })?;

        // Wait for an event with timeout (for tick)
        let event = tokio::time::timeout(tick_rate, event_rx.recv()).await;

        match event {
            // Received an event within the timeout
            Ok(Some(received_event)) => match received_event {
                Event::Key(key) => {
                    // When help is visible, intercept keys before the DFA.
                    // This is necessary because Esc maps to Quit in the DFA,
                    // but we want it to dismiss help instead.
                    if app.show_help {
                        match key.code {
                            KeyCode::Char('?') | KeyCode::Esc => {
                                app.toggle_help();
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
            // Channel closed (sender dropped)
            Ok(None) => {
                warn!("Event channel closed");
                break;
            }
            // Timeout - treat as tick for UI refresh
            Err(_) => {
                // Timeout expired, just continue to redraw
            }
        }

        // Check if we should quit
        if app.should_quit {
            cancel_token.cancel();
            break;
        }

        // Check if cancellation was requested externally
        if cancel_token.is_cancelled() {
            break;
        }
    }

    Ok(())
}

// ============================================================================
// Logging Setup
// ============================================================================

/// Returns the path to the log file directory.
///
/// Respects XDG Base Directory specification:
/// - Uses `$XDG_STATE_HOME/atm` if set
/// - Falls back to `$HOME/.local/state/atm`
fn get_log_dir() -> Option<PathBuf> {
    // Respect XDG_STATE_HOME first per XDG specification
    if let Ok(xdg_state) = std::env::var("XDG_STATE_HOME") {
        return Some(PathBuf::from(xdg_state).join("atm"));
    }
    // Fall back to HOME-based default
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".local/state/atm"))
}

/// Creates the log file for TUI logging.
///
/// Creates the log directory if it doesn't exist and opens the log file
/// in append mode. Returns `None` if any step fails (logging will be disabled).
///
/// Prints warnings to stderr before TUI takes over the terminal, so users
/// can see why logging might be unavailable.
fn create_log_file() -> Option<std::fs::File> {
    let log_dir = get_log_dir()?;

    // Create directory if it doesn't exist
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
    // 0. Parse CLI arguments
    let args = Args::parse();

    // Handle subcommands first (before TUI initialization)
    match args.command {
        Some(Command::Setup) => {
            return setup::setup();
        }
        Some(Command::Uninstall) => {
            return setup::uninstall();
        }
        None => {
            // Continue with normal TUI operation
        }
    }

    // 1. Initialize logging to file (stderr corrupts TUI display)
    // TUI apps cannot log to stderr because it writes to the same terminal,
    // interfering with the alternate screen buffer.
    let log_file = create_log_file();

    if let Some(file) = log_file {
        // Wrap in Mutex for thread-safe writes from async tasks
        let writer = Mutex::new(file);

        // Build filter with default directives
        // Note: "atm=info" is a compile-time constant guaranteed to parse successfully.
        let filter =
            EnvFilter::from_default_env().add_directive("atm=info".parse().unwrap_or_else(|_| {
                tracing_subscriber::filter::Directive::from(tracing::Level::INFO)
            }));

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(writer)
            .with_ansi(false) // No ANSI colors in log file
            .init();
    } else {
        // Fallback: no logging if file can't be created
        // This is acceptable for TUI - logging is non-critical and we warned the user above
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("off"))
            .init();
    }

    info!(pick_mode = args.pick, "ATM TUI starting...");

    // 1.5. In pick mode, fail fast if not in tmux
    if args.pick && !tmux::is_in_tmux() {
        bail!("--pick mode requires running inside tmux");
    }

    // 1.6. Ensure daemon is running (start it if not)
    if let Err(e) = daemon::ensure_daemon_running() {
        bail!("Failed to ensure daemon is running: {e}");
    }

    // 2. Create event channel for communication between tasks
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();

    // 3. Create command channel for TUI -> daemon client communication
    let (command_tx, command_rx) = mpsc::unbounded_channel::<ClientCommand>();

    // 4. Create cancellation token for graceful shutdown
    let cancel_token = CancellationToken::new();

    // 5. Initialize terminal
    let mut terminal = match setup_terminal() {
        Ok(t) => t,
        Err(e) => {
            error!(error = %e, "Failed to initialize terminal");
            return Err(e.into());
        }
    };

    // 6. Initialize application state
    let mut app = if args.pick {
        App::with_pick_mode()
    } else {
        App::new()
    };

    // 7. Spawn daemon client task
    let daemon_client =
        DaemonClient::with_defaults(event_tx.clone(), command_rx, cancel_token.clone());
    let daemon_handle = tokio::spawn(async move {
        daemon_client.run().await;
    });

    // 8. Spawn keyboard input task
    let keyboard_handle = spawn_keyboard_task(event_tx, cancel_token.clone());

    // 9. Run the main event loop
    let result = run_event_loop(
        &mut terminal,
        &mut app,
        &mut event_rx,
        &command_tx,
        &cancel_token,
    )
    .await;

    // 10. Signal shutdown to all tasks
    cancel_token.cancel();

    // 11. Wait for tasks to finish with timeout
    let _ = tokio::time::timeout(Duration::from_millis(100), daemon_handle).await;
    let _ = tokio::time::timeout(Duration::from_millis(100), keyboard_handle).await;

    // 12. Cleanup terminal (always, even on error)
    if let Err(e) = cleanup_terminal(&mut terminal) {
        error!(error = %e, "Failed to cleanup terminal");
        // Still return the original error if there was one
    }

    info!("ATM TUI stopped");

    // Return any error from the event loop
    result
}
