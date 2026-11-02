//! The TUI: terminal setup, the keyboard, capture, Beads and filter tasks, and the event loop.

use std::collections::HashSet;
use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use atm_tmux::{RealTmuxClient, TmuxClient};
use atm_tui::app::App;
use atm_tui::beads::BeadsReader;
use atm_tui::error::{Result as TuiResult, TuiError};
use atm_tui::input::{ClientCommand, Event};
use atm_tui::keybinding::{InputHandler, UiAction};
use atm_tui::tmux;
use atm_tui::ui;

use super::spawn::build_spawn_command;

// ============================================================================
// Terminal Setup / Cleanup
// ============================================================================

pub(super) fn setup_terminal() -> TuiResult<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().map_err(|e| TuiError::TerminalInit(e.to_string()))?;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| TuiError::TerminalInit(e.to_string()))?;

    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(|e| TuiError::TerminalInit(e.to_string()))
}

pub(super) fn cleanup_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> TuiResult<()> {
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

pub(super) fn spawn_keyboard_task(
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

/// Spawns a task that captures the selected agent's tmux pane output every
/// second, and at once when the selected pane changes.
pub(super) fn spawn_capture_task(
    client: impl TmuxClient + 'static,
    event_tx: mpsc::UnboundedSender<Event>,
    cancel_token: CancellationToken,
    mut capture_pane_rx: tokio::sync::watch::Receiver<Option<String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                changed = capture_pane_rx.changed() => {
                    // A closed channel stays ready forever, so stop on it
                    if changed.is_err() {
                        break;
                    }
                }
            }
            if cancel_token.is_cancelled() {
                break;
            }

            let pane_id = capture_pane_rx.borrow_and_update().clone();
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

/// Spawns a task that reads the selected session's in-progress Beads task
/// off the render path, so a slow or hung mount never stalls drawing.
pub(super) fn spawn_beads_task(
    event_tx: mpsc::UnboundedSender<Event>,
    cancel_token: CancellationToken,
    beads_dir_rx: tokio::sync::watch::Receiver<Option<String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BeadsReader::new(atm_tui::beads::find_in_progress_tasks);
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let poll_loop = async {
            loop {
                interval.tick().await;
                let dir = beads_dir_rx.borrow().clone();
                if let Some((dir, task)) = reader
                    .poll(dir.as_deref(), Duration::from_millis(500))
                    .await
                {
                    if event_tx.send(Event::BeadsUpdate { dir, task }).is_err() {
                        break;
                    }
                }
            }
        };
        // Stop mid-wait on cancel, so quitting never waits out a tick or a read.
        tokio::select! {
            () = cancel_token.cancelled() => {}
            () = poll_loop => {}
        }
    })
}

// ============================================================================
// Main Event Loop
// ============================================================================

/// Points the capture task at the selected session's pane when that pane
/// changed, dropping the old pane's output.
fn sync_capture_target(
    app: &mut App,
    capture_pane_tx: &tokio::sync::watch::Sender<Option<String>>,
) {
    let new_pane = app.selected_session().and_then(|s| s.tmux_pane.clone());
    if app.capture_pane_id != new_pane {
        app.capture_pane_id.clone_from(&new_pane);
        app.captured_output.clear();
        let _ = capture_pane_tx.send(new_pane);
    }
}

pub(super) async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    event_rx: &mut mpsc::UnboundedReceiver<Event>,
    command_tx: &mpsc::UnboundedSender<ClientCommand>,
    cancel_token: &CancellationToken,
    capture_pane_tx: &tokio::sync::watch::Sender<Option<String>>,
    beads_dir_tx: &tokio::sync::watch::Sender<Option<String>>,
) -> Result<()> {
    let tick_rate = Duration::from_millis(100);

    // Vim keybinding handler (DFA state machine)
    let mut handler = InputHandler::new();

    // Viewport height for half-page navigation (updated each frame)
    let mut viewport_height: u16 = 0;

    // How far the help popup can scroll at the current size (updated each frame)
    let mut help_max_scroll: u16 = 0;

    loop {
        app.tick();
        app.now = chrono::Utc::now();
        beads_dir_tx.send_replace(app.selected_beads_dir().map(str::to_owned));
        // Every frame, so any event that moves the selection moves the preview
        sync_capture_target(app, capture_pane_tx);

        // Render the UI and capture viewport height for half-page navigation
        terminal.draw(|frame| {
            help_max_scroll = ui::help_popup::max_scroll(frame.area());
            if app.compact {
                let layout = ui::layout::CompactLayout::new(frame.area());
                viewport_height = layout.list_area.height.saturating_sub(2);
                ui::render_compact(frame, app);
            } else {
                let layout = ui::layout::AppLayout::new(frame.area());
                viewport_height = layout.list_area.height.saturating_sub(2);
                ui::render(frame, app);
            }
        })?;
        // Keep the stored offset within range after a resize, so `k` is never dead.
        app.help_scroll = app.help_scroll.min(help_max_scroll);

        let event = tokio::time::timeout(tick_rate, event_rx.recv()).await;

        match event {
            Ok(Some(received_event)) => match received_event {
                Event::Key(key) => {
                    // When help is visible, intercept keys before the DFA.
                    // This is necessary because Esc maps to Quit in the DFA,
                    // but we want it to dismiss help instead.
                    if app.show_help {
                        // Same rule as `VimKeyResolver`: navigation only
                        // fires for plain keys (Shift allowed).
                        let plain = !key.modifiers.intersects(
                            KeyModifiers::CONTROL
                                | KeyModifiers::ALT
                                | KeyModifiers::SUPER
                                | KeyModifiers::HYPER
                                | KeyModifiers::META,
                        );
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
                            KeyCode::Char('j') | KeyCode::Down if plain => {
                                app.help_scroll =
                                    app.help_scroll.saturating_add(1).min(help_max_scroll);
                            }
                            KeyCode::Char('k') | KeyCode::Up if plain => {
                                app.help_scroll = app.help_scroll.saturating_sub(1);
                            }
                            _ => {} // Swallow all other keys
                        }
                    } else if app.search_active {
                        // The search prompt captures text, so `j`, `q` etc.
                        // edit the query instead of reaching the DFA.
                        match key.code {
                            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                                app.quit();
                                cancel_token.cancel();
                                break;
                            }
                            KeyCode::Esc => app.clear_filter(),
                            KeyCode::Enter => app.confirm_search(),
                            KeyCode::Backspace => app.pop_search_char(),
                            KeyCode::Char(c)
                                if !key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                            {
                                app.push_search_char(c);
                            }
                            _ => {}
                        }
                    } else if key.code == KeyCode::Esc && app.filter_matches.is_some() {
                        // Esc clears an active filter before it quits.
                        app.clear_filter();
                        handler.reset();
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
                                // Enter on a group row expands it if collapsed
                                // (open_fold is a no-op on already-expanded
                                // groups); on leaf rows, jump to the pane.
                                if let Some(session) = app.selected_session() {
                                    let session_id = session.id.clone();
                                    info!(session_id = %session_id, "Jump to session");
                                    if let Some(ref pane_id) = session.tmux_pane {
                                        let pane_id = pane_id.clone();
                                        match tmux::jump_to_pane(&RealTmuxClient::new(), &pane_id)
                                            .await
                                        {
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
                                } else {
                                    // Group row — toggle so Enter can close
                                    // an expanded group as well as open a
                                    // collapsed one.
                                    app.toggle_fold();
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
                            UiAction::StartSearch => app.start_search(),
                            UiAction::NextAttention => app.select_attention(true),
                            UiAction::PrevAttention => app.select_attention(false),
                            UiAction::ExpandNode => app.open_fold(),
                            UiAction::CloseFold => app.close_fold(),
                            UiAction::ToggleFold => app.toggle_fold(),
                            UiAction::CollapseAllFolds => app.collapse_all(),
                            UiAction::ExpandAllFolds => app.expand_all(),
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
                            UiAction::SpawnAgent
                            | UiAction::SpawnAgentLeft
                            | UiAction::SpawnAgentRight
                            | UiAction::SpawnAgentAbove
                            | UiAction::SpawnAgentBelow => {
                                if tmux::is_in_tmux() {
                                    let direction = match action {
                                        UiAction::SpawnAgentLeft => atm_tmux::PaneDirection::Left,
                                        UiAction::SpawnAgentRight => atm_tmux::PaneDirection::Right,
                                        UiAction::SpawnAgentAbove => atm_tmux::PaneDirection::Above,
                                        _ => atm_tmux::PaneDirection::Below,
                                    };
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
                                                current_session.is_none_or(|s| p.session_name == s)
                                                    && atm_pane.as_deref()
                                                        != Some(p.pane_id.as_str())
                                            })
                                            .max_by_key(|p| (p.width as u32) * (p.height as u32))
                                            .map(|p| p.pane_id.as_str())
                                            .or(atm_pane.as_deref())
                                            .unwrap_or("%0");
                                        // Query the target pane's cwd for the new agent
                                        let cwd = client.get_pane_cwd(target).await.ok().flatten();
                                        // Same builder cmd_spawn uses, so the
                                        // TUI keyboard shortcut and the
                                        // `atm spawn` subcommand stay in sync —
                                        // including ATM_SPAWN_BIN.
                                        let cmd = build_spawn_command(cwd.as_deref(), None);
                                        // Split without a command to get an interactive shell
                                        // (the user's PATH and aliases), then send-keys.
                                        match client
                                            .split_window(target, "50%", direction, None)
                                            .await
                                        {
                                            Ok(pane_id) => {
                                                if let Err(e) =
                                                    client.send_keys(&pane_id, &cmd).await
                                                {
                                                    warn!(error = %e, "Failed to send command to pane");
                                                } else if let Err(e) =
                                                    client.send_keys(&pane_id, "Enter").await
                                                {
                                                    warn!(error = %e, "Failed to send Enter to pane");
                                                }
                                                info!(pane_id = %pane_id, ?direction, "Spawned new agent")
                                            }
                                            Err(e) => warn!(error = %e, "Failed to spawn agent"),
                                        }
                                    });
                                }
                            }
                        }
                    }
                }
                Event::CaptureUpdate { pane_id, lines } => {
                    app.update_capture(&pane_id, lines);
                }
                Event::BeadsUpdate { dir, task } => {
                    app.update_beads_task(dir, task);
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
                Event::FilterUpdate(pane_ids) => {
                    app.update_filter_panes(pane_ids);
                }
                Event::DiscoveryComplete { discovered, failed } => {
                    info!(discovered, failed, "Discovery complete");
                }
                Event::DaemonDisconnected(reason) => {
                    warn!("Daemon disconnected");
                    app.mark_disconnected(reason);
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
// Filter Task
// ============================================================================

/// Spawns a task that periodically polls tmux for pane IDs belonging to the
/// filtered session and sends FilterUpdate events.
pub(super) fn spawn_filter_task(
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

#[cfg(test)]
mod capture_task_tests {
    use super::{spawn_capture_task, Event};
    use async_trait::async_trait;
    use atm_tmux::{MockTmuxClient, PaneDirection, PaneInfo, TmuxClient, TmuxError};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{mpsc, watch, Semaphore};
    use tokio::task::JoinHandle;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    struct Rig {
        client: MockTmuxClient,
        events: mpsc::UnboundedReceiver<Event>,
        target: watch::Sender<Option<String>>,
        cancel: CancellationToken,
        handle: JoinHandle<()>,
    }

    fn rig(start: Option<&str>) -> Rig {
        let client = MockTmuxClient::new();
        client.set_pane_content("%1", vec!["one".to_string()]);
        client.set_pane_content("%2", vec!["two".to_string()]);
        let (event_tx, events) = mpsc::unbounded_channel();
        let (target, target_rx) = watch::channel(start.map(str::to_string));
        let cancel = CancellationToken::new();
        let handle = spawn_capture_task(client.clone(), event_tx, cancel.clone(), target_rx);
        Rig {
            client,
            events,
            target,
            cancel,
            handle,
        }
    }

    /// The next capture within `within`, by pane id and first line. The clock
    /// is paused and the mock has no real I/O, so time moves only when
    /// every task is idle: this deadline and the 1 s tick can't race.
    async fn next_capture(rig: &mut Rig, within: Duration) -> Option<(String, String)> {
        match timeout(within, rig.events.recv()).await {
            Ok(Some(Event::CaptureUpdate { pane_id, lines })) => Some((pane_id, lines.join("\n"))),
            Ok(_) => panic!("expected a capture"),
            Err(_) => None,
        }
    }

    fn capture(pane: &str, text: &str) -> Option<(String, String)> {
        Some((pane.to_string(), text.to_string()))
    }

    const WELL_BEFORE_THE_TICK: Duration = Duration::from_millis(500);
    const PAST_THE_TICK: Duration = Duration::from_millis(1100);

    #[tokio::test(start_paused = true)]
    async fn a_new_target_is_captured_without_waiting_for_the_tick() {
        let mut rig = rig(Some("%1"));
        // The interval's first tick is immediate; this consumes it.
        assert_eq!(
            next_capture(&mut rig, WELL_BEFORE_THE_TICK).await,
            capture("%1", "one")
        );

        rig.target.send_replace(Some("%2".to_string()));

        assert_eq!(
            next_capture(&mut rig, WELL_BEFORE_THE_TICK).await,
            capture("%2", "two")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_selection_is_captured_without_waiting_for_the_tick() {
        let mut rig = rig(None);
        // Past the immediate first tick, which has nothing to capture.
        tokio::time::sleep(Duration::from_millis(10)).await;

        rig.target.send_replace(Some("%1".to_string()));

        assert_eq!(
            next_capture(&mut rig, WELL_BEFORE_THE_TICK).await,
            capture("%1", "one")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_target_keeps_refreshing_on_the_tick() {
        let mut rig = rig(Some("%1"));
        assert_eq!(
            next_capture(&mut rig, WELL_BEFORE_THE_TICK).await,
            capture("%1", "one")
        );

        assert_eq!(next_capture(&mut rig, WELL_BEFORE_THE_TICK).await, None);
        assert_eq!(
            next_capture(&mut rig, PAST_THE_TICK).await,
            capture("%1", "one")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn clearing_the_target_captures_nothing() {
        let mut rig = rig(Some("%1"));
        assert_eq!(
            next_capture(&mut rig, WELL_BEFORE_THE_TICK).await,
            capture("%1", "one")
        );

        rig.target.send_replace(None);

        assert_eq!(next_capture(&mut rig, PAST_THE_TICK).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_after_cancel_ends_the_task_without_capturing() {
        let mut rig = rig(None);
        tokio::time::sleep(Duration::from_millis(10)).await;

        rig.cancel.cancel();
        rig.target.send_replace(Some("%1".to_string()));

        // Ok(Ok(())): a panicking task would also finish the handle.
        assert!(matches!(
            timeout(PAST_THE_TICK, &mut rig.handle).await,
            Ok(Ok(()))
        ));
        assert!(rig.events.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_retarget_capture_retries_on_the_tick() {
        let mut rig = rig(Some("%1"));
        assert_eq!(
            next_capture(&mut rig, WELL_BEFORE_THE_TICK).await,
            capture("%1", "one")
        );

        rig.client.set_next_error(TmuxError::CommandFailed {
            command: "capture-pane".to_string(),
            stderr: "can't find pane".to_string(),
        });
        rig.target.send_replace(Some("%2".to_string()));

        assert_eq!(next_capture(&mut rig, WELL_BEFORE_THE_TICK).await, None);
        assert_eq!(
            next_capture(&mut rig, PAST_THE_TICK).await,
            capture("%2", "two")
        );
    }

    /// Holds each capture until the test releases it, so a target can change
    /// while one is in flight. `MockTmuxClient` answers without ever yielding.
    struct GatedClient {
        started: mpsc::UnboundedSender<String>,
        release: Arc<Semaphore>,
    }

    #[async_trait]
    impl TmuxClient for GatedClient {
        async fn capture_pane(&self, pane: &str) -> Result<Vec<String>, TmuxError> {
            let _ = self.started.send(pane.to_string());
            if let Ok(permit) = self.release.acquire().await {
                permit.forget();
            }
            Ok(vec![pane.to_string()])
        }

        async fn raw_command(&self, _: &str, _: &[&str]) -> Result<String, TmuxError> {
            unreachable!("the capture task only captures")
        }

        async fn split_window(
            &self,
            _: &str,
            _: &str,
            _: PaneDirection,
            _: Option<&str>,
        ) -> Result<String, TmuxError> {
            unreachable!("the capture task only captures")
        }

        async fn get_pane_cwd(&self, _: &str) -> Result<Option<String>, TmuxError> {
            unreachable!("the capture task only captures")
        }

        async fn new_window(&self, _: &str, _: Option<&str>) -> Result<String, TmuxError> {
            unreachable!("the capture task only captures")
        }

        async fn kill_pane(&self, _: &str) -> Result<(), TmuxError> {
            unreachable!("the capture task only captures")
        }

        async fn resize_pane(
            &self,
            _: &str,
            _: Option<u16>,
            _: Option<u16>,
        ) -> Result<(), TmuxError> {
            unreachable!("the capture task only captures")
        }

        async fn send_keys(&self, _: &str, _: &str) -> Result<(), TmuxError> {
            unreachable!("the capture task only captures")
        }

        async fn paste_text(&self, _: &str, _: &str) -> Result<(), TmuxError> {
            unreachable!("the capture task only captures")
        }

        async fn list_panes(&self) -> Result<Vec<PaneInfo>, TmuxError> {
            unreachable!("the capture task only captures")
        }

        async fn display_popup(&self, _: &str, _: &str, _: &str) -> Result<(), TmuxError> {
            unreachable!("the capture task only captures")
        }

        async fn select_pane(&self, _: &str) -> Result<(), TmuxError> {
            unreachable!("the capture task only captures")
        }

        async fn new_session(&self, _: &str) -> Result<String, TmuxError> {
            unreachable!("the capture task only captures")
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_target_changed_during_a_capture_is_captured_right_after_it() {
        let (started_tx, mut started) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let client = GatedClient {
            started: started_tx,
            release: Arc::clone(&release),
        };
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let (target, target_rx) = watch::channel(Some("%1".to_string()));
        let _handle = spawn_capture_task(client, event_tx, CancellationToken::new(), target_rx);
        // Every wait has a deadline: with the clock paused, a task that stopped
        // capturing would otherwise hang this test rather than fail it.
        assert_eq!(
            timeout(WELL_BEFORE_THE_TICK, started.recv()).await,
            Ok(Some("%1".to_string()))
        );

        // The capture of %1 is in flight when the target moves.
        target.send_replace(Some("%2".to_string()));
        release.add_permits(1);

        // %1's result still arrives; `App::update_capture` is what drops it.
        assert!(matches!(
            timeout(WELL_BEFORE_THE_TICK, events.recv()).await,
            Ok(Some(Event::CaptureUpdate { pane_id, .. })) if pane_id == "%1"
        ));
        // The clock can't reach the 1 s tick before this 500 ms deadline.
        assert_eq!(
            timeout(WELL_BEFORE_THE_TICK, started.recv()).await,
            Ok(Some("%2".to_string()))
        );
    }

    /// Real time, not paused: a task that spins on the closed channel keeps
    /// the runtime busy, and a paused clock would never reach this deadline.
    #[tokio::test]
    async fn a_dropped_sender_ends_the_task() {
        let rig = rig(None);

        drop(rig.target);

        let ended = timeout(Duration::from_secs(5), rig.handle).await;
        assert!(
            matches!(ended, Ok(Ok(()))),
            "the task must end cleanly, not spin, keep polling or panic"
        );
    }
}
