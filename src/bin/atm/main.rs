//! ATM — Agent Tmux Manager
//!
//! CLI + TUI for managing coding agents in tmux.
//!
//! # Usage
//!
//! ```text
//! atm                        # Launch TUI dashboard
//! atm --pick                 # Pick mode - exit after jumping to a session
//! atm spawn                  # Spawn a new coding agent
//! atm kill <session-id>      # Kill an agent
//! atm interrupt <session-id> # Send SIGINT to an agent
//! atm send <session-id> <text> # Send text to agent pane
//! atm list                   # List agents (tab-separated)
//! atm status                 # One-line summary for tmux status bar
//! atm setup                  # Configure agent integrations
//! atm uninstall              # Remove hooks
//! ```

use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

#[cfg(test)]
static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());
use tracing_subscriber::EnvFilter;

use atm_tmux::RealTmuxClient;
use atm_tui::app::App;
use atm_tui::client::DaemonClient;
use atm_tui::daemon;
use atm_tui::input::{ClientCommand, Event};
use atm_tui::setup;
use atm_tui::tmux;

mod cli;
mod spawn;
mod tui;
mod workspace;
use cli::{cmd_interrupt, cmd_kill, cmd_list, cmd_peek, cmd_reply, cmd_send, cmd_status};
use spawn::cmd_spawn;
use tui::{
    cleanup_terminal, run_event_loop, setup_terminal, spawn_beads_task, spawn_capture_task,
    spawn_filter_task, spawn_keyboard_task,
};
use workspace::{cmd_workspace, cmd_workspace_attach};

// ============================================================================
// CLI Arguments
// ============================================================================

/// ATM — Agent Tmux Manager for coding-agent harnesses
#[derive(Parser, Debug)]
#[command(name = "atm")]
#[command(about = "Manage coding agents in tmux")]
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

    /// Compact sidebar mode: vertical layout for narrow panes
    #[arg(long)]
    compact: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Configure supported harness integrations for atm
    Setup,
    /// Remove atm integrations from supported harnesses
    Uninstall,
    /// Spawn a coding agent in a tmux pane
    ///
    /// Splits the pane given by -t, else the pane atm runs in ($TMUX_PANE),
    /// and prints the new pane's id (e.g. %12) as soon as it exists.
    Spawn {
        /// Harness to launch (e.g., claude, pi, codex, amp, qwen, gemini). Defaults to [harness].default in config, or claude.
        #[arg(long = "harness", short = 'H')]
        harness: Option<String>,
        /// Model to use (e.g., opus, sonnet, haiku)
        #[arg(long, short = 'm')]
        model: Option<String>,
        /// Working directory for the new agent
        #[arg(long, short = 'w')]
        worktree: Option<String>,
        /// Split direction: left, right, above, below
        #[arg(long, short = 'd', default_value = "below")]
        direction: SpawnDirection,
        /// Size of the new pane (e.g., "30%", "50%")
        #[arg(long, short = 's', default_value = "50%")]
        size: String,
        /// Target pane to split from (e.g., "%5"). Defaults to $TMUX_PANE. Passed by tmux keybindings via #{pane_id}.
        #[arg(long, short = 't')]
        target_pane: Option<String>,
        /// Output format: the new pane's id, or JSON {pane, harness, cwd}
        #[arg(long, short = 'f', default_value = "text")]
        format: SpawnFormat,
        /// First prompt, passed as the agent's last argument (claude, pi and codex only). One line: put a long brief in a file
        #[arg(long)]
        prompt: Option<String>,
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
    ///
    /// Pastes the text, then presses Enter, as if the user typed it: `/clear`
    /// runs, and a half-typed draft in the input merges with the text. Refuses
    /// a session waiting on a dialog (answer it with `atm reply`), a subagent
    /// that shares its lead's pane, a pane in copy mode, and multi-line text
    /// for a program that didn't ask for bracketed paste (tmux 3.7 and later
    /// report this; with older tmux, multi-line text is sent).
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
    /// Launch or attach to a tmux workspace with ATM sidebar
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
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

#[derive(Debug, Clone, Subcommand)]
enum WorkspaceAction {
    /// Create a new tmux workspace with ATM sidebar, agent, and shell
    Create {
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
    /// Inject ATM sidebar into an existing tmux session
    Attach {
        /// Target session name (default: most recently attached session)
        session: Option<String>,
        /// Use isolated tmux server (-L atm-<name>)
        #[arg(long)]
        isolate: bool,
    },
}

#[derive(Debug, Clone, ValueEnum)]
enum SpawnDirection {
    Left,
    Right,
    Above,
    Below,
}

impl From<SpawnDirection> for atm_tmux::PaneDirection {
    fn from(d: SpawnDirection) -> Self {
        match d {
            SpawnDirection::Left => atm_tmux::PaneDirection::Left,
            SpawnDirection::Right => atm_tmux::PaneDirection::Right,
            SpawnDirection::Above => atm_tmux::PaneDirection::Above,
            SpawnDirection::Below => atm_tmux::PaneDirection::Below,
        }
    }
}

#[derive(Debug, Clone, ValueEnum)]
enum SpawnFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, ValueEnum)]
enum ListFormat {
    Table,
    Json,
    Ids,
}

// ============================================================================
// Logging Setup
// ============================================================================

fn get_log_dir() -> Option<PathBuf> {
    if let Some(xdg_state) = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return Some(PathBuf::from(xdg_state).join("atm"));
    }
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
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
            return Ok(setup::setup()?);
        }
        Some(Command::Uninstall) => {
            return Ok(setup::uninstall()?);
        }
        Some(Command::Spawn {
            harness,
            model,
            worktree,
            direction,
            size,
            target_pane,
            format,
            prompt,
        }) => {
            return cmd_spawn(
                harness,
                model,
                worktree,
                direction,
                size,
                target_pane,
                format,
                prompt,
            )
            .await;
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
        Some(Command::Workspace { action }) => {
            return match action {
                WorkspaceAction::Create {
                    name,
                    isolate,
                    editor,
                } => cmd_workspace(name, isolate, editor).await,
                WorkspaceAction::Attach { session, isolate } => {
                    cmd_workspace_attach(session, isolate).await
                }
            };
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
    app.compact = args.compact;

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

    // Spawn the Beads read task; its preview only shows in compact mode
    let (beads_dir_tx, beads_dir_rx) = tokio::sync::watch::channel(None::<String>);
    let beads_handle = app
        .compact
        .then(|| spawn_beads_task(event_tx.clone(), cancel_token.clone(), beads_dir_rx));

    // Create watch channel for selected pane ID and spawn capture polling task
    let (capture_pane_tx, capture_pane_rx) = tokio::sync::watch::channel(None::<String>);
    let capture_handle = spawn_capture_task(
        RealTmuxClient::new(),
        event_tx,
        cancel_token.clone(),
        capture_pane_rx,
    );

    let result = run_event_loop(
        &mut terminal,
        &mut app,
        &mut event_rx,
        &command_tx,
        &cancel_token,
        &capture_pane_tx,
        &beads_dir_tx,
    )
    .await;

    cancel_token.cancel();

    let _ = tokio::time::timeout(Duration::from_millis(100), daemon_handle).await;
    let _ = tokio::time::timeout(Duration::from_millis(100), keyboard_handle).await;
    let _ = tokio::time::timeout(Duration::from_millis(100), capture_handle).await;
    if let Some(handle) = filter_handle {
        let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
    }
    if let Some(handle) = beads_handle {
        let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
    }

    if let Err(e) = cleanup_terminal(&mut terminal) {
        error!(error = %e, "Failed to cleanup terminal");
    }

    info!("ATM TUI stopped");

    result
}
