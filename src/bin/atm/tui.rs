//! The TUI: terminal setup, the keyboard, capture, Beads and filter tasks, and the event loop.

use std::collections::{HashMap, HashSet};
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

use atm_protocol::{DaemonMessage, ProtocolVersion, ResponsePayload};
use atm_tmux::RealTmuxClient;
#[cfg(test)]
use atm_tmux::TmuxClient;
use atm_tui::app::App;
use atm_tui::beads::BeadsReader;
use atm_tui::error::{Result as TuiResult, TuiError};
use atm_tui::input::{ClientCommand, Event};
use atm_tui::keybinding::{InputHandler, UiAction};
use atm_tui::tmux;
use atm_tui::ui;
use atm_tui::{
    control::ControlNotice,
    daemon::ControlHandle,
    work::{WorkEffect, WorkInput},
};

use super::{
    rpc::CallerContext,
    spawn::{build_spawn_command_for_harness, default_spawn_harness, SpawnHarnessDefinition},
    tui_decisions::DecisionController,
    tui_work::WorkController,
};

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

/// Capture one snapshotted pane on the shared session connection, without replay.
async fn capture_preview(
    control: &ControlHandle,
    pane_id: String,
    caller: CallerContext,
) -> Option<Event> {
    match control.request(
        "session.capture".into(),
        serde_json::json!({"target": pane_id, "tail": null, "prompt": false, "caller_server": caller.caller_server}),
        caller.from_pane,
        Duration::from_secs(40),
    ).await {
        Ok(ResponsePayload::Result(value)) => {
            match serde_json::from_value::<Vec<String>>(value.get("lines").cloned().unwrap_or_default()) {
                Ok(lines) => Some(Event::CaptureUpdate { pane_id, lines }),
                Err(error) => {
                    warn!(error = %error, "Invalid preview capture response");
                    None
                }
            }
        }
        Ok(ResponsePayload::Error(error)) => {
            warn!(code = %error.code, error = %error.message, "Failed to capture preview");
            None
        }
        Err(error) => {
            warn!(error = %error, "Failed to capture preview; request was not retried");
            None
        }
    }
}

/// Spawns a task that captures the selected agent's tmux pane output every
/// second, and at once when the selected pane changes.
pub(super) fn spawn_capture_task(
    control: ControlHandle,
    event_tx: mpsc::UnboundedSender<Event>,
    cancel_token: CancellationToken,
    mut capture_pane_rx: tokio::sync::watch::Receiver<Option<String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let poll_loop = async {
            loop {
                tokio::select! {
                    biased;
                    changed = capture_pane_rx.changed() => {
                        // Drain the latest unseen target before stopping on closure.
                        if changed.is_err() { break; }
                    }
                    _ = interval.tick() => {}
                }
                let pane_id = capture_pane_rx.borrow_and_update().clone();
                if let Some(pane) = pane_id.filter(|pane| !pane.is_empty()) {
                    let caller = match CallerContext::from_env() {
                        Ok(caller) => caller,
                        Err(error) => {
                            warn!(error = %error, "Cannot resolve preview caller context");
                            continue;
                        }
                    };
                    // Await serially: selection changes never queue extra workers.
                    if let Some(event) = capture_preview(&control, pane, caller).await {
                        if event_tx.send(event).is_err() {
                            break;
                        }
                    }
                }
            }
        };
        tokio::select! {
            biased;
            () = cancel_token.cancelled() => { debug!("Preview capture cancelled"); }
            () = event_tx.closed() => {}
            () = poll_loop => {}
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

/// Snapshot the pane before spawning one bounded request on the session connection.
fn spawn_kill_selected_pane(
    app: &App,
    control: ControlHandle,
    caller: CallerContext,
    cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let pane_id = app.selected_session()?.tmux_pane.clone()?;
    info!(pane_id = %pane_id, "Killing agent pane");
    Some(tokio::spawn(async move {
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                warn!("Kill request cancelled; outcome may be unknown");
                return;
            }
            result = control.request(
                "session.kill".into(),
                serde_json::json!({"target": pane_id, "caller_server": caller.caller_server}),
                caller.from_pane,
                Duration::from_secs(40),
            ) => result,
        };
        match result {
            Ok(ResponsePayload::Result(_)) => {}
            Ok(ResponsePayload::Error(error)) => {
                warn!(code = %error.code, error = %error.message, "Failed to kill pane");
            }
            Err(error) => {
                warn!(error = %error, "Failed to kill pane; a sent request's outcome may be unknown");
            }
        }
    }))
}

/// Snapshot the selected pane and request Ctrl+C off the event loop.
fn spawn_interrupt_selected_pane(
    app: &App,
    control: ControlHandle,
    caller: CallerContext,
    cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let pane_id = app.selected_session()?.tmux_pane.clone()?;
    info!(pane_id = %pane_id, "Interrupting agent");
    Some(tokio::spawn(async move {
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                warn!("Interrupt request cancelled; outcome may be unknown");
                return;
            }
            result = control.request(
                "session.interrupt".into(),
                serde_json::json!({"target": pane_id, "caller_server": caller.caller_server}),
                caller.from_pane,
                Duration::from_secs(40),
            ) => result,
        };
        match result {
            Ok(ResponsePayload::Result(_)) => {}
            Ok(ResponsePayload::Error(error)) => {
                warn!(code = %error.code, error = %error.message, "Failed to interrupt");
            }
            Err(error) => {
                warn!(error = %error, "Failed to interrupt; a sent request's outcome may be unknown");
            }
        }
    }))
}

fn spawn_caller_cwd(cwd: io::Result<std::path::PathBuf>) -> Option<String> {
    match cwd.and_then(|path| {
        path.into_os_string().into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "TUI current directory is not valid UTF-8",
            )
        })
    }) {
        Ok(cwd) => Some(cwd),
        Err(error) => {
            warn!(error = %error, "Failed to resolve caller directory; spawning with daemon directory");
            None
        }
    }
}

/// Retains a single config read when a spawn worker times out or is cancelled.
struct SpawnHarnessReader<F> {
    load: F,
    pending: Option<tokio::sync::oneshot::Receiver<SpawnHarnessDefinition>>,
}
impl<F: Fn() -> SpawnHarnessDefinition + Clone + Send + 'static> SpawnHarnessReader<F> {
    async fn read(&mut self) -> Result<SpawnHarnessDefinition> {
        if self.pending.is_none() {
            let (send, receive) = tokio::sync::oneshot::channel();
            let load = self.load.clone();
            // A FIFO/hung filesystem must not pin runtime shutdown or accumulate threads.
            std::thread::Builder::new()
                .name("atm-spawn-config".into())
                .spawn(move || {
                    let _ = send.send(load());
                })?;
            self.pending = Some(receive);
        }
        let result = self
            .pending
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Missing spawn config read"))?
            .await;
        self.pending = None;
        result.map_err(|_| anyhow::anyhow!("Spawn config read ended without a result"))
    }
}
type SharedSpawnHarness =
    std::sync::Arc<tokio::sync::Mutex<SpawnHarnessReader<fn() -> SpawnHarnessDefinition>>>;
fn spawn_harness_reader() -> SharedSpawnHarness {
    std::sync::Arc::new(tokio::sync::Mutex::new(SpawnHarnessReader {
        load: default_spawn_harness as fn() -> SpawnHarnessDefinition,
        pending: None,
    }))
}

/// Inject both I/O boundaries so time tests use only in-memory futures.
#[allow(clippy::too_many_arguments)] // Frozen spawn inputs plus request/build boundaries.
async fn spawn_agent_sequence<F, Fut, B, Build>(
    caller_pane: Option<String>,
    caller_cwd: Option<String>,
    direction: atm_tmux::PaneDirection,
    deadline: tokio::time::Instant,
    spawned: &mut Option<String>,
    split_requested: &mut bool,
    mut request: F,
    build: B,
) -> Result<()>
where
    F: FnMut(&'static str, serde_json::Value, Duration) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value>>,
    B: FnOnce(Option<String>) -> Build,
    Build: std::future::Future<Output = Result<String>>,
{
    let remaining = || deadline.saturating_duration_since(tokio::time::Instant::now());
    let prepared = request(
        "session.spawn_prepare_largest",
        serde_json::json!({"caller_pane":caller_pane}),
        remaining(),
    )
    .await?;
    let target = prepared
        .get("target_pane")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Invalid spawn preparation: missing target_pane"))?;
    let cwd = match prepared.get("cwd") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(cwd)) => Some(cwd.as_str()),
        _ => anyhow::bail!("Invalid spawn preparation: cwd must be a string or null"),
    };
    // Preserve caller config/overrides and the original pre-split build point.
    let command = build(cwd.map(str::to_owned)).await?;
    *split_requested = true;
    let split = request("session.spawn", serde_json::json!({
        "target_pane":target,"caller_pane_missing":false,"direction":direction,"size":"50%","cwd":caller_cwd
    }), remaining()).await?;
    let pane = split
        .get("pane")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Invalid session.spawn response: missing pane"))?;
    let pane = atmd::control::reservation::PaneId::parse(pane.to_owned())?;
    *spawned = Some(pane.to_string());
    let result = request(
        "session.spawn_type",
        serde_json::json!({"pane":pane.as_str(),"command":command}),
        remaining(),
    )
    .await?;
    if !result.is_null() {
        anyhow::bail!("Invalid session.spawn_type response: expected null");
    }
    Ok(())
}

async fn spawn_agent_request(
    control: &ControlHandle,
    caller: &CallerContext,
    method: &'static str,
    mut params: serde_json::Value,
    budget: Duration,
) -> Result<serde_json::Value> {
    params
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Invalid spawn parameters"))?
        .insert(
            "caller_server".into(),
            serde_json::Value::String(caller.caller_server.clone()),
        );
    match control
        .request(method.into(), params, caller.from_pane.clone(), budget)
        .await?
    {
        ResponsePayload::Result(value) => Ok(value),
        ResponsePayload::Error(error) => {
            let message = format!("{} (daemon code: {})", error.message, error.code);
            Err(anyhow::Error::new(super::rpc::RpcError::Daemon(error)).context(message))
        }
    }
}

/// Preparation is read-only; later failures cannot undo an earlier split.
fn spawn_failure_outcome(
    split_requested: bool,
    spawned: bool,
    error: &anyhow::Error,
) -> &'static str {
    use atm_protocol::error_code;
    use atm_tui::control::ControlFailure;
    let did_nothing = !split_requested
        || matches!(
            error.downcast_ref::<ControlFailure>(),
            Some(
                ControlFailure::Unsupported(_)
                    | ControlFailure::DuplicateId
                    | ControlFailure::IdExhausted
            )
        )
        || matches!(error.downcast_ref::<super::rpc::RpcError>(), Some(super::rpc::RpcError::Daemon(error))
            if matches!(error.code.as_str(), error_code::REFUSED | error_code::INVALID_PARAMS | error_code::UNKNOWN_METHOD));
    match (spawned, did_nothing) {
        (false, true) => "failed request did nothing",
        (true, true) => "pane was already split; failed request did nothing",
        (false, false) => "a sent request's outcome may be unknown",
        (true, false) => "pane was already split; a sent request's outcome may be unknown",
    }
}

fn spawn_agent(
    control: ControlHandle,
    caller: CallerContext,
    caller_pane: Option<String>,
    caller_cwd: Option<String>,
    direction: atm_tmux::PaneDirection,
    cancel: CancellationToken,
    harness: SharedSpawnHarness,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(deadline) = tokio::time::Instant::now().checked_add(Duration::from_secs(40))
        else {
            warn!("Failed to spawn agent: deadline overflow");
            return;
        };
        let mut spawned = None;
        let mut split_requested = false;
        let flow = spawn_agent_sequence(
            caller_pane,
            caller_cwd,
            direction,
            deadline,
            &mut spawned,
            &mut split_requested,
            |method, params, budget| spawn_agent_request(&control, &caller, method, params, budget),
            move |cwd| async move {
                let harness = harness.lock().await.read().await?;
                Ok(build_spawn_command_for_harness(
                    &harness,
                    cwd.as_deref(),
                    None,
                ))
            },
        );
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(anyhow::anyhow!("Spawn request cancelled")),
            result = tokio::time::timeout_at(deadline, flow) => match result {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("Spawn request timed out")),
            },
        };
        if let Err(error) = result {
            let outcome = spawn_failure_outcome(split_requested, spawned.is_some(), &error);
            if spawned.is_some() {
                if error
                    .to_string()
                    .starts_with(atmd::control::SPAWN_ENTER_FAILURE_CONTEXT)
                {
                    warn!(error = %error, outcome, "Failed to send Enter to pane");
                } else {
                    warn!(error = %error, outcome, "Failed to send command to pane");
                }
            } else {
                warn!(error = %error, outcome, "Failed to spawn agent");
            }
        }
        if let Some(pane_id) = spawned {
            info!(pane_id = %pane_id, ?direction, "Spawned new agent");
        }
    })
}

/// Write one snapshotted optimistic role change without retry or local fallback.
fn spawn_role_set(
    control: ControlHandle,
    pane_id: String,
    on: bool,
    caller: CallerContext,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                warn!(%pane_id, "Role write cancelled; outcome may be unknown");
                return;
            }
            result = control.request(
                "session.role_set".into(),
                serde_json::json!({"pane":pane_id,"role":on.then_some("coordinator"),"caller_server":caller.caller_server}),
                caller.from_pane,
                Duration::from_secs(40),
            ) => result,
        };
        match result {
            Ok(ResponsePayload::Result(_)) => {}
            Ok(ResponsePayload::Error(error)) => {
                warn!(code=%error.code, error=%error.message, %pane_id, "Failed to toggle coordinator tag")
            }
            Err(error) => {
                warn!(%error, %pane_id, "Failed to toggle coordinator tag; outcome may be unknown; request was not retried")
            }
        }
    })
}

fn execute_work_effects(
    app: &mut App,
    work: &mut WorkController,
    decisions: &mut DecisionController,
    mut effects: Vec<WorkEffect>,
) {
    loop {
        if let Some(seq) = app.work.org().and_then(|org| org.seq) {
            decisions.advance_work_cursor(seq);
        }
        match work.execute(effects, decisions) {
            Some(input) => effects = app.reduce_work(input, work.now()),
            None => break,
        }
    }
}
fn apply_work_input(
    app: &mut App,
    work: &mut WorkController,
    decisions: &mut DecisionController,
    input: WorkInput,
) {
    let effects = app.reduce_work(input, work.now());
    execute_work_effects(app, work, decisions, effects);
}
fn session_panes(app: &App) -> HashMap<atm_core::SessionId, Option<String>> {
    app.sessions
        .values()
        .map(|session| (session.id.clone(), session.tmux_pane.clone()))
        .collect()
}

#[allow(clippy::too_many_arguments)] // Existing TUI channels plus their shared control handle.
pub(super) async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    event_rx: &mut mpsc::UnboundedReceiver<Event>,
    command_tx: &mpsc::UnboundedSender<ClientCommand>,
    control: &ControlHandle,
    cancel_token: &CancellationToken,
    capture_pane_tx: &tokio::sync::watch::Sender<Option<String>>,
    beads_dir_tx: &tokio::sync::watch::Sender<Option<String>>,
) -> Result<()> {
    let tick_rate = Duration::from_millis(100);
    let mut kill_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut interrupt_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut spawn_task: Option<tokio::task::JoinHandle<()>> = None;
    let spawn_harness = spawn_harness_reader();
    let spawn_cancel = cancel_token.child_token();
    let _spawn_cancel_on_drop = spawn_cancel.clone().drop_guard();
    let mut role_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut decisions =
        DecisionController::new(control.clone(), super::spawn::non_empty_env("TMUX_PANE"));
    decisions.refresh();
    let mut work = WorkController::new(control.clone(), super::spawn::non_empty_env("TMUX_PANE"));

    // Vim keybinding handler (DFA state machine)
    let mut handler = InputHandler::new();

    // Viewport height for half-page navigation (updated each frame)
    let mut viewport_height: u16 = 0;

    // How far the help popup can scroll at the current size (updated each frame)
    let mut help_max_scroll: u16 = 0;

    loop {
        app.decision_panes.clone_from(&decisions.waiting_panes);
        app.tick();
        app.now = chrono::Utc::now();
        app.work_now = work.now();
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
            decisions.render(frame);
        })?;
        // Keep the stored offset within range after a resize, so `k` is never dead.
        app.help_scroll = app.help_scroll.min(help_max_scroll);

        let event = tokio::select! {
            update = decisions.recv() => {
                if let Some(update) = update { decisions.apply(update); }
                continue;
            }
            input = work.recv() => {
                apply_work_input(app, &mut work, &mut decisions, input);
                continue;
            }
            event = tokio::time::timeout(tick_rate, event_rx.recv()) => event,
        };

        match event {
            Ok(Some(received_event)) => match received_event {
                Event::Key(key) => {
                    // When help is visible, intercept keys before the DFA.
                    // This is necessary because Esc maps to Quit in the DFA,
                    // but we want it to dismiss help instead.
                    if decisions.popup.is_some() {
                        handler.reset();
                        if decisions.key(key) {
                            app.quit();
                            cancel_token.cancel();
                            break;
                        }
                    } else if app.show_help {
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
                            UiAction::ToggleWorkView => {
                                let effects = app.toggle_work_view(work.now());
                                execute_work_effects(app, &mut work, &mut decisions, effects);
                            }
                            UiAction::ToggleWorkGrouping => {
                                if let Some(effects) = app.toggle_work_grouping(work.now()) {
                                    execute_work_effects(app, &mut work, &mut decisions, effects);
                                }
                            }
                            UiAction::GoToTab(index) => {
                                if let Some(effects) = app.select_tab(index, work.now()) {
                                    execute_work_effects(app, &mut work, &mut decisions, effects);
                                }
                            }
                            UiAction::Refresh => {
                                apply_work_input(
                                    app,
                                    &mut work,
                                    &mut decisions,
                                    WorkInput::Refresh,
                                );
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
                            UiAction::OpenDecision => {
                                if key.kind == crossterm::event::KeyEventKind::Press {
                                    handler.reset();
                                    decisions.open_for(app.selected_session());
                                }
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
                            UiAction::ToggleCoordinator => {
                                if role_task.as_ref().is_some_and(|task| !task.is_finished()) {
                                    warn!("Role write already in flight; ignoring keypress");
                                } else if let Some((pane, on)) = app.toggle_selected_coordinator() {
                                    match CallerContext::from_env() {
                                        Ok(caller) => {
                                            role_task = Some(spawn_role_set(
                                                control.clone(),
                                                pane,
                                                on,
                                                caller,
                                                cancel_token.clone(),
                                            ))
                                        }
                                        Err(error) => {
                                            warn!(%error, "Failed to resolve role caller; optimistic tag retained")
                                        }
                                    }
                                }
                            }
                            UiAction::KillAgent => {
                                if kill_task.as_ref().is_some_and(|task| !task.is_finished()) {
                                    warn!("Kill request already in flight; ignoring keypress");
                                } else {
                                    match CallerContext::from_env() {
                                        Ok(caller) => {
                                            kill_task = spawn_kill_selected_pane(
                                                app,
                                                control.clone(),
                                                caller,
                                                cancel_token.clone(),
                                            );
                                        }
                                        Err(error) => {
                                            warn!(error = %error, "Failed to resolve kill caller")
                                        }
                                    }
                                }
                            }
                            UiAction::InterruptAgent => {
                                if interrupt_task
                                    .as_ref()
                                    .is_some_and(|task| !task.is_finished())
                                {
                                    warn!("Interrupt request already in flight; ignoring keypress");
                                } else {
                                    match CallerContext::from_env() {
                                        Ok(caller) => {
                                            interrupt_task = spawn_interrupt_selected_pane(
                                                app,
                                                control.clone(),
                                                caller,
                                                cancel_token.clone(),
                                            );
                                        }
                                        Err(error) => {
                                            warn!(error = %error, "Failed to resolve interrupt caller")
                                        }
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
                                    if spawn_task.as_ref().is_some_and(|task| !task.is_finished()) {
                                        warn!("Spawn request already in flight; ignoring keypress");
                                    } else {
                                        let caller_pane = std::env::var("TMUX_PANE").ok();
                                        let caller_cwd = spawn_caller_cwd(std::env::current_dir());
                                        match CallerContext::from_env() {
                                            Ok(caller) => {
                                                spawn_task = Some(spawn_agent(
                                                    control.clone(),
                                                    caller,
                                                    caller_pane,
                                                    caller_cwd,
                                                    direction,
                                                    spawn_cancel.clone(),
                                                    spawn_harness.clone(),
                                                ))
                                            }
                                            Err(error) => {
                                                warn!(error = %error, "Failed to spawn agent")
                                            }
                                        }
                                    }
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
                    let before = session_panes(app);
                    app.update_sessions(sessions);
                    if before != session_panes(app) {
                        apply_work_input(app, &mut work, &mut decisions, WorkInput::PanesChanged);
                    }
                }
                Event::SessionListReplace(sessions) => {
                    debug!(count = sessions.len(), "Received full session list");
                    let before = session_panes(app);
                    app.replace_sessions(sessions);
                    if before != session_panes(app) {
                        apply_work_input(app, &mut work, &mut decisions, WorkInput::PanesChanged);
                    }
                }
                Event::SessionRemoved(session_id) => {
                    debug!(session_id = %session_id, "Session removed");
                    let before = session_panes(app);
                    app.remove_session(&session_id);
                    if before != session_panes(app) {
                        apply_work_input(app, &mut work, &mut decisions, WorkInput::PanesChanged);
                    }
                }
                Event::RoleUpdate(panes) => {
                    app.update_coordinator_panes(panes);
                }
                Event::FilterUpdate(pane_ids) => {
                    app.update_filter_panes(pane_ids);
                }
                Event::DiscoveryComplete { discovered, failed } => {
                    info!(discovered, failed, "Discovery complete");
                }
                Event::Control(notice) => {
                    decisions.notice(&notice);
                    let input = match notice {
                        ControlNotice::Capabilities {
                            protocol_version,
                            methods,
                        } => Some(WorkInput::Capabilities {
                            methods,
                            watch: protocol_version.major == ProtocolVersion::CURRENT.major
                                && protocol_version.minor >= 1,
                        }),
                        ControlNotice::Event(DaemonMessage::Event { seq, kind, .. }) => {
                            Some(WorkInput::WatchEvent { seq, kind })
                        }
                        ControlNotice::WatchFailed(_) => Some(WorkInput::WatchFailed),
                        _ => None,
                    };
                    if let Some(input) = input {
                        apply_work_input(app, &mut work, &mut decisions, input);
                    }
                }
                Event::DaemonDisconnected(reason) => {
                    decisions.disconnected();
                    apply_work_input(app, &mut work, &mut decisions, WorkInput::WatchFailed);
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

    work.shutdown();
    decisions.shutdown();
    if let Some(task) = kill_task {
        task.abort();
    }
    if let Some(task) = interrupt_task {
        task.abort();
    }
    if let Some(task) = spawn_task {
        task.abort();
    }
    if let Some(task) = role_task {
        task.abort();
    }
    Ok(())
}

// ============================================================================
// Filter Task
// ============================================================================

/// One bounded observation; failure publishes nothing, unlike an empty success.
async fn filter_session_panes(
    control: &ControlHandle,
    session_name: &str,
    caller: CallerContext,
) -> Option<HashSet<String>> {
    match control
        .request(
            "session.panes".into(),
            serde_json::json!({"session_name":session_name,"caller_server":caller.caller_server}),
            caller.from_pane,
            Duration::from_secs(40),
        )
        .await
    {
        Ok(ResponsePayload::Result(value)) => {
            let ids = value
                .get("pane_ids")
                .cloned()
                .and_then(|ids| serde_json::from_value(ids).ok());
            if ids.is_none() {
                warn!("Invalid session pane membership response");
            }
            ids
        }
        Ok(ResponsePayload::Error(error)) => {
            warn!(code = %error.code, error = %error.message, "Failed to read session panes");
            None
        }
        Err(error) => {
            warn!(error = %error, "Failed to read session panes");
            None
        }
    }
}

/// Poll exact membership through the existing session connection, one read at a time.
pub(super) fn spawn_filter_task(
    session_name: String,
    control: ControlHandle,
    event_tx: mpsc::UnboundedSender<Event>,
    cancel_token: CancellationToken,
    mut caller_context: impl FnMut() -> Result<CallerContext, super::rpc::RpcError> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let poll_loop = async {
            loop {
                interval.tick().await;
                let caller = match caller_context() {
                    Ok(caller) => caller,
                    Err(error) => {
                        warn!(error = %error, "Cannot resolve session filter caller context");
                        continue;
                    }
                };
                if let Some(pane_ids) = filter_session_panes(&control, &session_name, caller).await
                {
                    if event_tx.send(Event::FilterUpdate(pane_ids)).is_err() {
                        break;
                    }
                }
            }
        };
        tokio::select! {
            biased;
            () = cancel_token.cancelled() => {}
            () = event_tx.closed() => {}
            () = poll_loop => {}
        }
    })
}

/// Read exact tags through the shared connection; classification stays local.
async fn read_roles(control: &ControlHandle, caller: CallerContext) -> Option<Event> {
    match control
        .request(
            "session.roles".into(),
            serde_json::json!({"caller_server":caller.caller_server}),
            caller.from_pane,
            Duration::from_secs(40),
        )
        .await
    {
        Ok(ResponsePayload::Result(value)) => match serde_json::from_value::<HashSet<String>>(
            value.get("coordinator_panes").cloned().unwrap_or_default(),
        ) {
            Ok(panes) => Some(Event::RoleUpdate(panes)),
            Err(error) => {
                warn!(%error, "Invalid pane role response");
                None
            }
        },
        Ok(ResponsePayload::Error(error)) => {
            warn!(code=%error.code, error=%error.message, "Failed to read pane roles");
            None
        }
        Err(error) => {
            warn!(%error, "Failed to read pane roles; request was not retried");
            None
        }
    }
}

/// Polls pane roles independently of the optional tmux-session filter.
/// Errors leave the last successful (or optimistic) role set on screen.
pub(super) fn spawn_role_task(
    control: ControlHandle,
    event_tx: mpsc::UnboundedSender<Event>,
    cancel_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let poll_loop = async {
            loop {
                interval.tick().await;
                let caller = match CallerContext::from_env() {
                    Ok(caller) => caller,
                    Err(error) => {
                        warn!(%error, "Cannot resolve role caller context");
                        continue;
                    }
                };
                if let Some(event) = read_roles(&control, caller).await {
                    if event_tx.send(event).is_err() {
                        break;
                    }
                }
            }
        };
        tokio::select! {
            biased;
            () = cancel_token.cancelled() => {}
            () = event_tx.closed() => {}
            () = poll_loop => {}
        }
    })
}

#[cfg(test)]
mod kill_action_tests {
    use super::super::rpc::CallerContext;
    use super::*;
    use atm_core::{SessionView, TreeRowKind};
    use atm_protocol::{ClientMessage, DaemonMessage, MessageType, ProtocolVersion};
    use atm_tui::{
        client::{DaemonClient, DaemonConfig},
        daemon::ControlHandle,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::{
            unix::{OwnedReadHalf, OwnedWriteHalf},
            UnixListener,
        },
    };

    const GUARD: Duration = Duration::from_secs(60);

    async fn guarded<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(GUARD, future)
            .await
            .expect("hung kill proof")
    }

    fn caller() -> CallerContext {
        CallerContext {
            from_pane: Some("%7".into()),
            caller_server: "/private/caller-server".into(),
        }
    }

    #[derive(Clone, Default)]
    struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Logs {
        fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
            let logs = Self::default();
            let writer = logs.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || writer.clone())
                .finish();
            (logs, tracing::subscriber::set_default(subscriber))
        }
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    struct Peer {
        reader: BufReader<OwnedReadHalf>,
        writer: OwnedWriteHalf,
    }
    impl Peer {
        async fn recv(&mut self) -> MessageType {
            let mut line = String::new();
            assert_ne!(guarded(self.reader.read_line(&mut line)).await.unwrap(), 0);
            serde_json::from_str::<ClientMessage>(&line)
                .unwrap()
                .message
        }
        async fn send(&mut self, message: DaemonMessage) {
            guarded(
                self.writer.write_all(
                    format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes(),
                ),
            )
            .await
            .unwrap();
        }
        async fn kill(&mut self) -> String {
            let MessageType::Request {
                id,
                method,
                params,
                from_pane,
            } = self.recv().await
            else {
                panic!("kill request")
            };
            assert_eq!(method, "session.kill");
            assert_eq!(
                params,
                serde_json::json!({"target": "%42", "caller_server": "/private/caller-server"})
            );
            assert_eq!(from_pane.as_deref(), Some("%7"));
            id
        }
    }

    struct Rig {
        _dir: tempfile::TempDir,
        listener: UnixListener,
        commands: mpsc::UnboundedSender<ClientCommand>,
        event_tx: mpsc::UnboundedSender<Event>,
        events: mpsc::UnboundedReceiver<Event>,
        cancel: CancellationToken,
        run: tokio::task::JoinHandle<()>,
    }
    impl Rig {
        async fn start(minor: u16, methods: &[&str]) -> (Self, Peer) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("peer.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let (commands, command_rx) = mpsc::unbounded_channel();
            let (event_tx, events) = mpsc::unbounded_channel();
            let cancel = CancellationToken::new();
            let client = DaemonClient::new(
                DaemonConfig {
                    socket_path: path,
                    ..Default::default()
                },
                event_tx.clone(),
                command_rx,
                cancel.clone(),
            );
            let run = tokio::spawn(async move { client.run().await });
            let mut rig = Self {
                _dir: dir,
                listener,
                commands,
                event_tx,
                events,
                cancel,
                run,
            };
            let peer = rig.connect(minor, methods).await;
            (rig, peer)
        }
        fn control(&self) -> ControlHandle {
            ControlHandle::new(self.commands.clone())
        }
        async fn connect(&mut self, minor: u16, methods: &[&str]) -> Peer {
            let (stream, _) = guarded(self.listener.accept()).await.unwrap();
            let (reader, writer) = stream.into_split();
            let mut peer = Peer {
                reader: BufReader::new(reader),
                writer,
            };
            assert!(matches!(peer.recv().await, MessageType::Connect { .. }));
            peer.send(DaemonMessage::Connected {
                protocol_version: ProtocolVersion::new(1, minor),
                client_id: "kill-peer".into(),
                methods: methods.iter().map(|m| (*m).into()).collect(),
            })
            .await;
            assert!(matches!(
                peer.recv().await,
                MessageType::Subscribe { session_id: None }
            ));
            peer.send(DaemonMessage::session_list(vec![])).await;
            loop {
                match guarded(self.events.recv()).await.unwrap() {
                    Event::Control(_) => {}
                    Event::SessionListReplace(sessions) if sessions.is_empty() => break,
                    other => panic!("initial session flow: {other:?}"),
                }
            }
            peer
        }
        async fn stop(&mut self) {
            self.cancel.cancel();
            guarded(&mut self.run).await.unwrap();
        }
    }
    impl Drop for Rig {
        fn drop(&mut self) {
            self.cancel.cancel();
            self.run.abort();
        }
    }

    fn selected_app(pane: Option<&str>) -> App {
        let mut app = App::new();
        app.view = atm_tui::app::AppView::Sessions;
        app.update_sessions(vec![SessionView {
            id: "selected".into(),
            project_root: Some("/project".into()),
            tmux_pane: pane.map(str::to_owned),
            ..SessionView::default()
        }]);
        app.selected_index = app
            .tree_rows
            .iter()
            .position(|row| matches!(row.kind, TreeRowKind::Agent { .. }))
            .unwrap();
        app
    }

    #[tokio::test]
    async fn no_selection_does_not_start_a_kill() {
        let mut group = selected_app(Some("%42"));
        group.select_go_to(0);
        assert!(group.selected_session().is_none());
        let mut out_of_bounds = selected_app(Some("%42"));
        out_of_bounds.selected_index = out_of_bounds.tree_rows.len();
        for app in [App::new(), group, out_of_bounds] {
            let (tx, mut rx) = mpsc::unbounded_channel();
            assert!(spawn_kill_selected_pane(
                &app,
                ControlHandle::new(tx),
                caller(),
                CancellationToken::new()
            )
            .is_none());
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn selected_session_without_a_pane_does_not_start_a_kill() {
        let app = selected_app(None);
        assert!(app.selected_session().is_some());
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(spawn_kill_selected_pane(
            &app,
            ControlHandle::new(tx),
            caller(),
            CancellationToken::new()
        )
        .is_none());
        assert!(rx.try_recv().is_err());
    }

    // Breaks if the worker resolves the selection after a refresh or sends a
    // session id instead of the pane that was captured before spawning.
    #[tokio::test]
    async fn kill_snapshots_the_selected_pane_before_spawning() {
        let (mut rig, mut peer) = Rig::start(1, &["session.kill"]).await;
        let mut app = selected_app(Some("%42"));
        let action =
            spawn_kill_selected_pane(&app, rig.control(), caller(), rig.cancel.clone()).unwrap();
        app.replace_sessions(Vec::new());
        let id = peer.kill().await;
        peer.send(DaemonMessage::response_ok(
            id,
            serde_json::json!({"pane": "%42"}),
        ))
        .await;
        guarded(action).await.unwrap();
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
        rig.stop().await;
    }

    #[tokio::test]
    async fn daemon_error_finishes_the_background_action_without_retrying() {
        let (logs, _guard) = Logs::capture();
        let (mut rig, mut peer) = Rig::start(1, &["session.kill"]).await;
        let app = selected_app(Some("%42"));
        let action =
            spawn_kill_selected_pane(&app, rig.control(), caller(), rig.cancel.clone()).unwrap();
        let id = peer.kill().await;
        peer.send(DaemonMessage::response_err(id, "refused", "wrong server"))
            .await;
        guarded(action).await.unwrap();
        assert!(logs.text().contains("refused"), "{}", logs.text());
        assert!(logs.text().contains("wrong server"), "{}", logs.text());
        assert!(
            app.selected_session().is_some(),
            "no optimistic row removal"
        );
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
        rig.stop().await;
    }

    #[tokio::test]
    async fn kill_unsupported_and_closed_connections_warn_without_fallback() {
        let (logs, _guard) = Logs::capture();
        let (mut rig, mut peer) = Rig::start(1, &[]).await;
        let app = selected_app(Some("%42"));
        guarded(
            spawn_kill_selected_pane(&app, rig.control(), caller(), rig.cancel.clone()).unwrap(),
        )
        .await
        .unwrap();
        assert!(
            logs.text().contains("daemon does not support session.kill"),
            "{}",
            logs.text()
        );
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
        rig.stop().await;
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        guarded(
            spawn_kill_selected_pane(
                &app,
                ControlHandle::new(tx),
                caller(),
                CancellationToken::new(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        assert!(
            logs.text().contains("control channel closed"),
            "{}",
            logs.text()
        );
    }

    async fn abandoned_command_is_not_sent(command: ClientCommand) {
        let (mut rig, mut peer) = Rig::start(1, &["session.kill"]).await;
        rig.commands.send(command).unwrap();
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
        rig.stop().await;
    }

    #[tokio::test]
    async fn kill_already_cancelled_never_queues_a_request() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        cancel.cancel();
        guarded(
            spawn_kill_selected_pane(
                &selected_app(Some("%42")),
                ControlHandle::new(tx),
                caller(),
                cancel,
            )
            .unwrap(),
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn kill_timeout_includes_offline_queue_and_never_sends_it_later() {
        let (logs, _guard) = Logs::capture();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let action = spawn_kill_selected_pane(
            &selected_app(Some("%42")),
            ControlHandle::new(tx),
            caller(),
            CancellationToken::new(),
        )
        .unwrap();
        let command = rx.recv().await.unwrap();
        tokio::time::advance(Duration::from_secs(40)).await;
        guarded(action).await.unwrap();
        assert!(
            logs.text().contains("outcome may be unknown"),
            "{}",
            logs.text()
        );
        assert!(rx.try_recv().is_err(), "timeout queued a retry");
        // All deadline work above is in-memory. Resume before any real I/O.
        tokio::time::resume();
        abandoned_command_is_not_sent(command).await;
    }

    #[tokio::test]
    async fn kill_cancelled_in_the_queue_is_not_sent_later() {
        let (logs, _guard) = Logs::capture();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let action = spawn_kill_selected_pane(
            &selected_app(Some("%42")),
            ControlHandle::new(tx),
            caller(),
            cancel.clone(),
        )
        .unwrap();
        let command = guarded(rx.recv()).await.unwrap();
        cancel.cancel();
        guarded(action).await.unwrap();
        assert!(
            logs.text().contains("outcome may be unknown"),
            "{}",
            logs.text()
        );
        abandoned_command_is_not_sent(command).await;
    }

    #[tokio::test]
    async fn kill_disconnect_warns_and_does_not_replay() {
        let (logs, _guard) = Logs::capture();
        let (mut rig, mut peer) = Rig::start(1, &["session.kill"]).await;
        let action = spawn_kill_selected_pane(
            &selected_app(Some("%42")),
            rig.control(),
            caller(),
            rig.cancel.clone(),
        )
        .unwrap();
        peer.kill().await;
        drop(peer);
        guarded(action).await.unwrap();
        assert!(
            logs.text().contains("request was not retried"),
            "{}",
            logs.text()
        );
        loop {
            match guarded(rig.events.recv()).await.unwrap() {
                Event::Control(_) => {}
                Event::DaemonDisconnected(_) => break,
                other => panic!("disconnect event: {other:?}"),
            }
        }
        let mut peer = rig.connect(1, &["session.kill"]).await;
        rig.commands.send(ClientCommand::Discover).unwrap();
        assert!(matches!(peer.recv().await, MessageType::Discover));
        rig.stop().await;
    }

    // The reply is deliberately NEVER released. Navigation, a second kill,
    // discovery and quit must all complete while it remains held by the peer.
    #[tokio::test]
    async fn kill_stall_keeps_navigation_discover_and_quit_live() {
        let (logs, _guard) = Logs::capture();
        let (mut rig, mut peer) = Rig::start(1, &["session.kill"]).await;
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
            },
        )
        .unwrap();
        let mut app = selected_app(Some("%42"));
        let (capture, _capture_rx) = tokio::sync::watch::channel(None);
        let (beads, _beads_rx) = tokio::sync::watch::channel(None);
        let control = rig.control();
        let input = rig.event_tx.clone();
        let key = |c| {
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            ))
        };
        input.send(key('x')).unwrap();
        let script = async {
            let MessageType::Request {
                method,
                params,
                from_pane,
                ..
            } = peer.recv().await
            else {
                panic!("kill request")
            };
            assert_eq!(method, "session.kill");
            assert_eq!(params.get("target"), Some(&serde_json::json!("%42")));
            assert!(params
                .get("caller_server")
                .and_then(|v| v.as_str())
                .is_some_and(|s| std::path::Path::new(s).is_absolute()));
            assert_eq!(
                from_pane,
                std::env::var("TMUX_PANE")
                    .ok()
                    .filter(|pane| !pane.is_empty())
            );
            input.send(key('x')).unwrap();
            input.send(key('k')).unwrap();
            input.send(key('r')).unwrap();
            // Discover proves input progress while the first reply is held.
            // The busy warning below positively proves duplicate rejection;
            // spawned worker scheduling alone cannot order it before Discover.
            assert!(matches!(peer.recv().await, MessageType::Discover));
            input.send(key('q')).unwrap();
        };
        let (result, ()) = guarded(async {
            tokio::join!(
                run_event_loop(
                    &mut terminal,
                    &mut app,
                    &mut rig.events,
                    &rig.commands,
                    &control,
                    &rig.cancel,
                    &capture,
                    &beads
                ),
                script
            )
        })
        .await;
        result.unwrap();
        assert_eq!(
            logs.text().matches("already in flight").count(),
            1,
            "{}",
            logs.text()
        );
        assert!(app.should_quit);
        assert!(
            app.selected_session().is_none(),
            "navigation did not reach the group row"
        );
        rig.stop().await;
    }

    mod role_request_tests {
        use super::*;

        async fn role_frame(peer: &mut Peer, method: &str, role: Option<&str>) -> String {
            let MessageType::Request {
                id,
                method: actual,
                params,
                from_pane,
            } = peer.recv().await
            else {
                panic!("role request")
            };
            assert_eq!(actual, method);
            let expected = if method == "session.roles" {
                serde_json::json!({"caller_server":"/private/caller-server"})
            } else {
                serde_json::json!({"caller_server":"/private/caller-server","pane":"%42","role":role})
            };
            assert_eq!(params, expected);
            assert_eq!(from_pane.as_deref(), Some("%7"));
            id
        }

        // Removing strict decoding or publishing empty on a failed read would
        // destroy the last successful/optimistic tags in this actual App.
        #[tokio::test]
        async fn role_reads_preserve_context_and_retain_tags_on_every_failure() {
            let (mut rig, mut peer) = Rig::start(1, &["session.roles"]).await;
            let mut app = selected_app(Some("%42"));
            for (value, expected) in [
                (
                    serde_json::json!({"coordinator_panes":["%42"]}),
                    Some(HashSet::from(["%42".into()])),
                ),
                (
                    serde_json::json!({"coordinator_panes":[]}),
                    Some(HashSet::new()),
                ),
                (serde_json::json!(null), None),
                (serde_json::json!({}), None),
                (serde_json::json!({"coordinator_panes":null}), None),
                (serde_json::json!({"coordinator_panes":["%7",1]}), None),
            ] {
                app.update_coordinator_panes(HashSet::from(["%old".into()]));
                let control = rig.control();
                let context = caller();
                let action = tokio::spawn(async move { read_roles(&control, context).await });
                let id = role_frame(&mut peer, "session.roles", None).await;
                peer.send(DaemonMessage::response_ok(id, value)).await;
                let event = guarded(action).await.unwrap();
                match (event, expected) {
                    (Some(Event::RoleUpdate(panes)), Some(expected)) => {
                        assert_eq!(panes, expected);
                        app.update_coordinator_panes(panes);
                        assert_eq!(app.coordinator_panes, expected);
                    }
                    (None, None) => {
                        assert_eq!(app.coordinator_panes, HashSet::from(["%old".into()]))
                    }
                    other => panic!("bad role decode: {other:?}"),
                }
            }
            let control = rig.control();
            let action = tokio::spawn(async move { read_roles(&control, caller()).await });
            let id = role_frame(&mut peer, "session.roles", None).await;
            peer.send(DaemonMessage::response_err(id, "refused", "foreign server"))
                .await;
            assert!(guarded(action).await.unwrap().is_none());
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
            rig.stop().await;
        }

        // The actual polling task must forward both nonempty and empty
        // successful reads, using the current environment's caller context.
        #[tokio::test]
        async fn role_poll_forwards_first_success_including_empty() {
            for panes in [serde_json::json!(["%42"]), serde_json::json!([])] {
                let (mut rig, mut peer) = Rig::start(1, &["session.roles"]).await;
                let (tx, mut events) = mpsc::unbounded_channel();
                let cancel = CancellationToken::new();
                let task = spawn_role_task(rig.control(), tx, cancel.clone());
                let MessageType::Request {
                    id,
                    method,
                    params,
                    from_pane,
                } = peer.recv().await
                else {
                    panic!("role poll")
                };
                let context = CallerContext::from_env().unwrap();
                assert_eq!(method, "session.roles");
                assert_eq!(
                    params,
                    serde_json::json!({"caller_server":context.caller_server})
                );
                assert_eq!(from_pane, context.from_pane);
                peer.send(DaemonMessage::response_ok(
                    id,
                    serde_json::json!({"coordinator_panes":panes}),
                ))
                .await;
                let Some(Event::RoleUpdate(actual)) = guarded(events.recv()).await else {
                    panic!("missing role update")
                };
                assert_eq!(
                    actual,
                    serde_json::from_value::<HashSet<String>>(panes).unwrap()
                );
                cancel.cancel();
                guarded(task).await.unwrap();
                rig.stop().await;
            }
        }

        // A later selection or failed write must not retarget/undo the
        // immediate optimistic toggle; only a successful poll reconciles it.
        #[tokio::test]
        async fn role_write_snapshots_optimism_and_failed_write_keeps_it() {
            let (mut rig, mut peer) = Rig::start(1, &["session.role_set"]).await;
            for (on, refused) in [(true, false), (false, false), (true, true)] {
                let mut app = selected_app(Some("%42"));
                if !on {
                    app.update_coordinator_panes(HashSet::from(["%42".into()]));
                }
                let (pane, desired) = app.toggle_selected_coordinator().unwrap();
                assert_eq!(desired, on);
                let action =
                    spawn_role_set(rig.control(), pane, desired, caller(), rig.cancel.clone());
                app.update_sessions(vec![SessionView {
                    id: "selected".into(),
                    tmux_pane: Some("%99".into()),
                    ..SessionView::default()
                }]);
                let id =
                    role_frame(&mut peer, "session.role_set", on.then_some("coordinator")).await;
                if refused {
                    peer.send(DaemonMessage::response_err(id, "refused", "foreign server"))
                        .await;
                } else {
                    peer.send(DaemonMessage::response_ok(
                        id,
                        serde_json::json!({"pane":"%42","role":on.then_some("coordinator")}),
                    ))
                    .await;
                }
                guarded(action).await.unwrap();
                assert_eq!(app.coordinator_panes.contains("%42"), on);
                app.update_coordinator_panes(HashSet::new());
                assert!(app.coordinator_panes.is_empty());
                rig.commands.send(ClientCommand::Discover).unwrap();
                assert!(matches!(peer.recv().await, MessageType::Discover));
            }
            rig.stop().await;
        }

        #[tokio::test]
        async fn role_unsupported_and_closed_connections_have_no_local_fallback() {
            let (logs, _guard) = Logs::capture();
            let (mut rig, mut peer) = Rig::start(1, &["session.kill"]).await;
            assert!(read_roles(&rig.control(), caller()).await.is_none());
            guarded(spawn_role_set(
                rig.control(),
                "%42".into(),
                true,
                caller(),
                rig.cancel.clone(),
            ))
            .await
            .unwrap();
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
            rig.stop().await;
            let (tx, rx) = mpsc::unbounded_channel();
            drop(rx);
            assert!(read_roles(&ControlHandle::new(tx.clone()), caller())
                .await
                .is_none());
            guarded(spawn_role_set(
                ControlHandle::new(tx),
                "%42".into(),
                false,
                caller(),
                CancellationToken::new(),
            ))
            .await
            .unwrap();
            assert!(logs.text().contains("does not support session.roles"));
            assert!(logs.text().contains("does not support session.role_set"));
            assert!(logs.text().contains("control channel closed"));
        }

        #[tokio::test(start_paused = true)]
        async fn role_write_timeout_and_cancel_never_replay_queued_commands() {
            let mut abandoned = vec![];
            for timed_out in [false, true] {
                let (tx, mut rx) = mpsc::unbounded_channel();
                let cancel = CancellationToken::new();
                let action = spawn_role_set(
                    ControlHandle::new(tx),
                    "%42".into(),
                    true,
                    caller(),
                    cancel.clone(),
                );
                let command = rx.recv().await.unwrap();
                if timed_out {
                    tokio::time::advance(Duration::from_secs(40)).await;
                } else {
                    cancel.cancel();
                }
                guarded(action).await.unwrap();
                assert!(rx.try_recv().is_err());
                abandoned.push(command);
            }
            tokio::time::resume();
            let (mut rig, mut peer) = Rig::start(1, &["session.role_set"]).await;
            for command in abandoned {
                rig.commands.send(command).unwrap();
            }
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
            rig.stop().await;
        }

        #[tokio::test]
        async fn an_already_canceled_role_write_never_queues() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let cancel = CancellationToken::new();
            cancel.cancel();
            guarded(spawn_role_set(
                ControlHandle::new(tx),
                "%42".into(),
                true,
                caller(),
                cancel,
            ))
            .await
            .unwrap();
            assert!(rx.try_recv().is_err());
        }

        #[tokio::test]
        #[ignore = "requires sealed private atmd/tmux and explicit role-check inputs"]
        async fn real_role_helpers_preserve_tags_and_parent_classification() {
            use atm_tui::client::DaemonClient;
            let root = std::path::PathBuf::from(std::env::var_os("ATM_ROLES_TEST_RUN").unwrap())
                .canonicalize()
                .unwrap();
            assert!(root.starts_with("/var/tmp"));
            assert!(root
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("green-"));
            assert!(std::env::var_os("TMUX").is_none());
            assert!(std::env::var_os("TMUX_PANE").is_none());
            for key in ["ATM_SOCKET", "TMUX_TMPDIR", "ATM_STATE_DIR", "HOME"] {
                assert!(std::path::PathBuf::from(std::env::var_os(key).unwrap()).starts_with(&root));
            }
            let caller = CallerContext::from_env().unwrap();
            assert!(std::path::Path::new(&caller.caller_server)
                .canonicalize()
                .unwrap()
                .starts_with(&root));
            let pane = std::env::var("ATM_ROLES_TEST_PANE").unwrap();
            let foreign = std::env::var("ATM_ROLES_TEST_FOREIGN").unwrap();
            assert!(std::path::Path::new(&foreign).starts_with(&root));
            let (commands, command_rx) = mpsc::unbounded_channel();
            let control = ControlHandle::new(commands);
            let (events, _events_rx) = mpsc::unbounded_channel();
            let cancel = CancellationToken::new();
            let daemon = DaemonClient::with_defaults(events, command_rx, cancel.clone());
            let connection = tokio::spawn(async move { daemon.run().await });
            let mut app = App::new();
            app.view = atm_tui::app::AppView::Sessions;
            app.update_sessions(vec![
                SessionView {
                    id: "lead".into(),
                    tmux_pane: Some(pane.clone()),
                    ..SessionView::default()
                },
                SessionView {
                    id: "child".into(),
                    tmux_pane: Some(pane.clone()),
                    parent_session_id: Some("lead".into()),
                    ..SessionView::default()
                },
            ]);
            for on in [true, false, false] {
                app.selected_index = app.tree_rows.iter().position(|row| matches!(&row.kind, TreeRowKind::Agent { session } if session.id.as_str() == "lead")).unwrap();
                let target = if app.coordinator_panes.contains(&pane) != on {
                    let (target, desired) = app.toggle_selected_coordinator().unwrap();
                    assert_eq!(desired, on);
                    target
                } else {
                    pane.clone()
                };
                guarded(spawn_role_set(
                    control.clone(),
                    target,
                    on,
                    caller.clone(),
                    cancel.clone(),
                ))
                .await
                .unwrap();
                let (tx, mut updates) = mpsc::unbounded_channel();
                let poll_cancel = CancellationToken::new();
                let poll = spawn_role_task(control.clone(), tx, poll_cancel.clone());
                let Some(Event::RoleUpdate(panes)) = guarded(updates.recv()).await else {
                    panic!("real role poll")
                };
                assert_eq!(panes.contains(&pane), on);
                app.update_coordinator_panes(panes);
                assert_eq!(
                    app.is_coordinator(app.sessions.get(&"lead".into()).unwrap()),
                    on
                );
                assert!(!app.is_coordinator(app.sessions.get(&"child".into()).unwrap()));
                poll_cancel.cancel();
                guarded(poll).await.unwrap();
            }
            let keeper = std::env::var("ATM_ROLES_TEST_KEEPER").unwrap();
            app.update_sessions(vec![
                SessionView {
                    id: "mate".into(),
                    tmux_pane: Some(keeper.clone()),
                    parent_session_id: Some("lead".into()),
                    ..SessionView::default()
                },
                SessionView {
                    id: "mate-child".into(),
                    tmux_pane: Some(keeper.clone()),
                    parent_session_id: Some("mate".into()),
                    ..SessionView::default()
                },
            ]);
            guarded(spawn_role_set(
                control.clone(),
                keeper.clone(),
                true,
                caller.clone(),
                cancel.clone(),
            ))
            .await
            .unwrap();
            let Some(Event::RoleUpdate(panes)) = read_roles(&control, caller.clone()).await else {
                panic!("teammate tag read")
            };
            app.update_coordinator_panes(panes);
            assert!(app.is_coordinator(app.sessions.get(&"mate".into()).unwrap()));
            assert!(!app.is_coordinator(app.sessions.get(&"mate-child".into()).unwrap()));
            guarded(spawn_role_set(
                control.clone(),
                keeper,
                false,
                caller.clone(),
                cancel.clone(),
            ))
            .await
            .unwrap();
            app.update_coordinator_panes(HashSet::from([pane.clone()]));
            let other = CallerContext {
                caller_server: foreign,
                from_pane: None,
            };
            assert!(read_roles(&control, other.clone()).await.is_none());
            assert!(app.coordinator_panes.contains(&pane));
            guarded(spawn_role_set(
                control.clone(),
                pane.clone(),
                true,
                other,
                cancel.clone(),
            ))
            .await
            .unwrap();
            let Some(Event::RoleUpdate(panes)) = read_roles(&control, caller).await else {
                panic!("own role read after foreign refusal")
            };
            assert!(!panes.contains(&pane));
            assert!(
                app.coordinator_panes.contains(&pane),
                "failed foreign write rolled back optimism"
            );
            app.update_coordinator_panes(panes);
            assert!(!app.coordinator_panes.contains(&pane));
            cancel.cancel();
            guarded(connection).await.unwrap();
        }

        // The first role reply is never released. Another c must be ignored,
        // while navigation, discovery and quit still finish without rollback.
        #[tokio::test]
        async fn role_stall_keeps_optimism_navigation_discover_and_quit_live() {
            let (logs, _guard) = Logs::capture();
            let (mut rig, mut peer) = Rig::start(1, &["session.role_set"]).await;
            let mut terminal = Terminal::with_options(
                CrosstermBackend::new(io::stdout()),
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
                },
            )
            .unwrap();
            let mut app = selected_app(Some("%42"));
            let (capture, _capture_rx) = tokio::sync::watch::channel(None);
            let (beads, _beads_rx) = tokio::sync::watch::channel(None);
            let control = rig.control();
            let input = rig.event_tx.clone();
            let key = |c| {
                Event::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Char(c),
                    KeyModifiers::NONE,
                ))
            };
            input.send(key('c')).unwrap();
            let script = async {
                let MessageType::Request {
                    method,
                    params,
                    from_pane,
                    ..
                } = peer.recv().await
                else {
                    panic!("role write bypassed daemon")
                };
                assert_eq!(method, "session.role_set");
                assert_eq!(params.get("pane"), Some(&serde_json::json!("%42")));
                assert_eq!(params.get("role"), Some(&serde_json::json!("coordinator")));
                let context = CallerContext::from_env().unwrap();
                assert_eq!(
                    params.get("caller_server"),
                    Some(&serde_json::json!(context.caller_server))
                );
                assert_eq!(from_pane, context.from_pane);
                input.send(key('c')).unwrap();
                input.send(key('k')).unwrap();
                input.send(key('r')).unwrap();
                assert!(matches!(peer.recv().await, MessageType::Discover));
                input.send(key('q')).unwrap();
            };
            let (result, ()) = guarded(async {
                tokio::join!(
                    run_event_loop(
                        &mut terminal,
                        &mut app,
                        &mut rig.events,
                        &rig.commands,
                        &control,
                        &rig.cancel,
                        &capture,
                        &beads
                    ),
                    script
                )
            })
            .await;
            result.unwrap();
            assert!(
                app.coordinator_panes.contains("%42"),
                "second c toggled while first was held"
            );
            assert_eq!(
                logs.text().matches("Role write already in flight").count(),
                1,
                "{}",
                logs.text()
            );
            assert!(app.should_quit);
            assert!(app.selected_session().is_none());
            rig.stop().await;
        }
    }

    mod spawn_request_tests {
        use super::*;
        use serde_json::{json, Value};
        const METHODS: [&str; 3] = [
            "session.spawn_prepare_largest",
            "session.spawn",
            "session.spawn_type",
        ];

        fn action(
            rig: &Rig,
            caller_pane: Option<&str>,
            direction: atm_tmux::PaneDirection,
        ) -> tokio::task::JoinHandle<()> {
            spawn_agent(
                rig.control(),
                caller(),
                caller_pane.map(str::to_owned),
                Some("/caller process directory".into()),
                direction,
                rig.cancel.clone(),
                spawn_harness_reader(),
            )
        }
        async fn request(peer: &mut Peer, expected: &str) -> (String, Value) {
            let MessageType::Request {
                id,
                method,
                params,
                from_pane,
            } = peer.recv().await
            else {
                panic!("spawn request")
            };
            assert_eq!(method, expected);
            assert_eq!(params["caller_server"], "/private/caller-server");
            assert_eq!(from_pane.as_deref(), Some("%7"));
            (id, params)
        }
        async fn prepare(peer: &mut Peer, caller_pane: Option<&str>, result: Value) {
            let (id, params) = request(peer, METHODS[0]).await;
            assert_eq!(
                params,
                json!({"caller_server":"/private/caller-server","caller_pane":caller_pane})
            );
            peer.send(DaemonMessage::response_ok(id, result)).await;
        }
        async fn sentinel(rig: &Rig, peer: &mut Peer) {
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(
                matches!(peer.recv().await, MessageType::Discover),
                "spawn retried or continued after failure"
            );
        }

        // Inject only Enter failure; the real daemon helper still builds its context.
        struct FailEnter(atm_tmux::MockTmuxClient);
        type Call<'a, T> = std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<T, atm_tmux::TmuxError>> + Send + 'a>,
        >;
        macro_rules! forward {
            ($method:ident <$($lt:lifetime),*> ($($arg:ident : $ty:ty),*) -> $result:ty) => {
                fn $method<'s, $($lt,)* 'async_trait>(&'s self, $($arg: $ty),*) -> Call<'async_trait, $result>
                where 's: 'async_trait, $($lt: 'async_trait,)* Self: 'async_trait {
                    Box::pin(self.0.$method($($arg),*))
                }
            };
        }
        impl TmuxClient for FailEnter {
            fn send_keys<'s, 'p, 'k, 'async_trait>(
                &'s self,
                pane: &'p str,
                keys: &'k str,
            ) -> Call<'async_trait, ()>
            where
                's: 'async_trait,
                'p: 'async_trait,
                'k: 'async_trait,
                Self: 'async_trait,
            {
                Box::pin(async move {
                    self.0.send_keys(pane, keys).await?;
                    if keys == "Enter" {
                        Err(atm_tmux::TmuxError::NotFound)
                    } else {
                        Ok(())
                    }
                })
            }
            forward!(raw_command<'a, 'b, 'c>(command: &'a str, args: &'b [&'c str]) -> String);
            forward!(split_window<'a, 'b, 'c, 'd>(target: &'a str, size: &'b str, direction: atm_tmux::PaneDirection, command: Option<&'c str>, cwd: Option<&'d str>, detached: bool) -> String);
            forward!(get_pane_cwd<'a>(pane: &'a str) -> Option<String>);
            forward!(new_window<'a, 'b>(session: &'a str, command: Option<&'b str>) -> String);
            forward!(kill_pane<'a>(pane: &'a str) -> ());
            forward!(resize_pane<'a>(pane: &'a str, width: Option<u16>, height: Option<u16>) -> ());
            forward!(paste_text<'a, 'b>(pane: &'a str, text: &'b str) -> ());
            forward!(list_panes<>() -> Vec<atm_tmux::PaneInfo>);
            forward!(display_popup<'a, 'b, 'c>(width: &'a str, height: &'b str, command: &'c str) -> ());
            forward!(select_pane<'a>(pane: &'a str) -> ());
            forward!(capture_pane<'a>(pane: &'a str) -> Vec<String>);
            forward!(new_session<'a>(name: &'a str) -> String);
        }

        #[tokio::test]
        async fn spawn_warning_uses_actual_daemon_enter_context() {
            use std::error::Error;
            let mock = atm_tmux::MockTmuxClient::new();
            let error = atmd::control::type_spawn_command(&FailEnter(mock.clone()), "%99", "agent")
                .await
                .unwrap_err();
            assert!(error
                .to_string()
                .starts_with(atmd::control::SPAWN_ENTER_FAILURE_CONTEXT));
            assert_eq!(mock.calls().len(), 2, "command must precede failed Enter");
            let message = format!("{}\nCaused by: {}", error, error.source().unwrap());
            let (logs, _guard) = Logs::capture();
            let (mut rig, mut peer) = Rig::start(1, &METHODS).await;
            let work = action(&rig, Some("%7"), atm_tmux::PaneDirection::Below);
            prepare(
                &mut peer,
                Some("%7"),
                json!({"target_pane":"%42","cwd":null}),
            )
            .await;
            let (id, _) = request(&mut peer, METHODS[1]).await;
            peer.send(DaemonMessage::response_ok(id, json!({"pane":"%99"})))
                .await;
            let (id, _) = request(&mut peer, METHODS[2]).await;
            peer.send(DaemonMessage::response_err(id, "internal", &message))
                .await;
            guarded(work).await.unwrap();
            let text = logs.text();
            assert!(text.contains("Failed to send Enter to pane"), "{text}");
            assert!(!text.contains("Failed to send command to pane"), "{text}");
            sentinel(&rig, &mut peer).await;
            rig.stop().await;
        }

        #[test]
        fn spawn_caller_directory_preserves_utf8_and_warns_once_on_each_failure() {
            use std::os::unix::ffi::OsStringExt;
            let (logs, _guard) = Logs::capture();
            assert_eq!(
                spawn_caller_cwd(Ok("/caller's path".into())).as_deref(),
                Some("/caller's path")
            );
            assert!(logs.text().is_empty());
            assert_eq!(
                spawn_caller_cwd(Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "caller directory unavailable"
                ))),
                None
            );
            let invalid = std::ffi::OsString::from_vec(b"/invalid-\xff".to_vec());
            assert_eq!(spawn_caller_cwd(Ok(invalid.into())), None);
            let text = logs.text();
            assert_eq!(
                text.matches("Failed to resolve caller directory").count(),
                2,
                "{text}"
            );
            assert!(text.contains("caller directory unavailable"), "{text}");
            assert!(text.contains("not valid UTF-8"), "{text}");
        }

        #[tokio::test]
        async fn spawn_exact_stages_context_directions_and_caller_built_command() {
            for (direction, name, caller_pane) in [
                (atm_tmux::PaneDirection::Left, "left", Some("%7")),
                (atm_tmux::PaneDirection::Right, "right", None),
                (atm_tmux::PaneDirection::Above, "above", Some("")),
                (atm_tmux::PaneDirection::Below, "below", Some("%999")),
            ] {
                let (mut rig, mut peer) = Rig::start(1, &METHODS).await;
                let work = action(&rig, caller_pane, direction);
                prepare(
                    &mut peer,
                    caller_pane,
                    json!({"target_pane":"%42","cwd":"/chosen's cwd"}),
                )
                .await;
                let (id, params) = request(&mut peer, METHODS[1]).await;
                assert_eq!(
                    params,
                    json!({"caller_server":"/private/caller-server","target_pane":"%42","caller_pane_missing":false,"direction":name,"size":"50%","cwd":"/caller process directory"})
                );
                peer.send(DaemonMessage::response_ok(id, json!({"pane":"%99"})))
                    .await;
                let (id, params) = request(&mut peer, METHODS[2]).await;
                assert_eq!(params["pane"], "%99");
                assert!(
                    params["command"]
                        .as_str()
                        .unwrap()
                        .starts_with("cd '/chosen'\\''s cwd' && "),
                    "{params}"
                );
                peer.send(DaemonMessage::session_removed("while-spawn-stalls".into()))
                    .await;
                loop {
                    match guarded(rig.events.recv()).await.unwrap() {
                        Event::Control(_) => {}
                        Event::SessionRemoved(id) if id == "while-spawn-stalls" => break,
                        other => panic!("spawn blocked sessions {other:?}"),
                    }
                }
                peer.send(DaemonMessage::response_ok(id, Value::Null)).await;
                guarded(work).await.unwrap();
                sentinel(&rig, &mut peer).await;
                rig.stop().await;
            }
        }

        #[tokio::test]
        async fn spawn_null_caller_and_unknown_directories_keep_fallbacks_without_retry() {
            let (mut rig, mut peer) = Rig::start(1, &METHODS).await;
            let work = spawn_agent(
                rig.control(),
                caller(),
                None,
                None,
                atm_tmux::PaneDirection::Below,
                rig.cancel.clone(),
                spawn_harness_reader(),
            );
            prepare(&mut peer, None, json!({"target_pane":"%0","cwd":null})).await;
            let (id, params) = request(&mut peer, METHODS[1]).await;
            assert_eq!(params["target_pane"], "%0");
            assert_eq!(params["cwd"], Value::Null);
            peer.send(DaemonMessage::response_ok(id, json!({"pane":"%99"})))
                .await;
            let (id, params) = request(&mut peer, METHODS[2]).await;
            assert!(!params["command"].as_str().unwrap().starts_with("cd "));
            peer.send(DaemonMessage::response_ok(id, Value::Null)).await;
            guarded(work).await.unwrap();
            sentinel(&rig, &mut peer).await;
            rig.stop().await;
        }

        #[test]
        fn spawn_failure_outcomes_use_typed_codes_and_conservative_stage_facts() {
            use super::super::super::rpc::RpcError;
            use atm_protocol::ResponseError;
            use atm_tui::control::ControlFailure;
            let check = |error: anyhow::Error, definite: bool| {
                assert_eq!(
                    spawn_failure_outcome(false, false, &error),
                    "failed request did nothing"
                );
                assert_eq!(
                    spawn_failure_outcome(true, false, &error),
                    if definite {
                        "failed request did nothing"
                    } else {
                        "a sent request's outcome may be unknown"
                    }
                );
                assert_eq!(
                    spawn_failure_outcome(true, true, &error),
                    if definite {
                        "pane was already split; failed request did nothing"
                    } else {
                        "pane was already split; a sent request's outcome may be unknown"
                    }
                );
            };
            for code in ["refused", "invalid_params", "unknown_method"] {
                check(
                    anyhow::Error::new(RpcError::Daemon(ResponseError {
                        code: code.into(),
                        message: "timeout-looking message".into(),
                    }))
                    .context("outer context"),
                    true,
                );
            }
            for code in ["timeout", "internal", "unavailable", "future_code"] {
                check(
                    anyhow::Error::new(RpcError::Daemon(ResponseError {
                        code: code.into(),
                        message: "refused invalid_params unknown_method".into(),
                    })),
                    false,
                );
            }
            for (error, definite) in [
                (ControlFailure::Unsupported("session.spawn".into()), true),
                (ControlFailure::DuplicateId, true),
                (ControlFailure::IdExhausted, true),
                (ControlFailure::Timeout, false),
                (ControlFailure::Disconnected, false),
                (ControlFailure::Closed, false),
            ] {
                check(error.into(), definite);
            }
            for message in [
                "Spawn request cancelled",
                "Spawn request timed out",
                "Invalid session.spawn response",
            ] {
                check(anyhow::anyhow!(message), false);
            }
        }

        // Break caught: definite daemon rejections are mislabeled as uncertain effects.
        #[tokio::test]
        async fn spawn_definite_failures_keep_prior_split_but_do_not_claim_unknown() {
            for (stage, method) in METHODS.iter().enumerate() {
                for code in ["refused", "invalid_params", "unknown_method"] {
                    let (logs, _guard) = Logs::capture();
                    let (mut rig, mut peer) = Rig::start(1, &METHODS).await;
                    let work = action(&rig, Some("%7"), atm_tmux::PaneDirection::Below);
                    for (n, method) in METHODS.iter().take(stage).enumerate() {
                        let (id, _) = request(&mut peer, method).await;
                        peer.send(DaemonMessage::response_ok(
                            id,
                            if n == 0 {
                                json!({"target_pane":"%42","cwd":null})
                            } else {
                                json!({"pane":"%99"})
                            },
                        ))
                        .await;
                    }
                    let (id, _) = request(&mut peer, method).await;
                    peer.send(DaemonMessage::response_err(id, code, "definite refusal"))
                        .await;
                    guarded(work).await.unwrap();
                    let text = logs.text();
                    assert!(
                        text.contains("did nothing"),
                        "stage {stage}, {code}: {text}"
                    );
                    assert!(!text.contains("outcome may be unknown"), "{text}");
                    assert_eq!(
                        text.contains("pane was already split"),
                        stage == 2,
                        "{text}"
                    );
                    assert!(
                        text.contains(code),
                        "typed daemon code still visible: {text}"
                    );
                    sentinel(&rig, &mut peer).await;
                    rig.stop().await;
                }
            }
        }

        #[tokio::test]
        async fn spawn_malformed_preparation_or_split_never_types() {
            for (stage, value) in [
                (0, Value::Null),
                (0, json!({})),
                (0, json!({"target_pane":"%42"})),
                (0, json!({"target_pane":7,"cwd":null})),
                (0, json!({"target_pane":"%42","cwd":[]})),
                (1, Value::Null),
                (1, json!({})),
                (1, json!({"pane":7})),
                (1, json!({"pane":"%00"})),
                (1, json!({"pane":"%99;"})),
            ] {
                let (logs, _guard) = Logs::capture();
                let (mut rig, mut peer) = Rig::start(1, &METHODS).await;
                let work = action(&rig, Some("%7"), atm_tmux::PaneDirection::Below);
                if stage == 0 {
                    prepare(&mut peer, Some("%7"), value).await;
                } else {
                    prepare(
                        &mut peer,
                        Some("%7"),
                        json!({"target_pane":"%42","cwd":null}),
                    )
                    .await;
                    let (id, _) = request(&mut peer, METHODS[1]).await;
                    peer.send(DaemonMessage::response_ok(id, value)).await;
                }
                guarded(work).await.unwrap();
                assert!(
                    logs.text().contains("Failed to spawn agent"),
                    "{}",
                    logs.text()
                );
                sentinel(&rig, &mut peer).await;
                rig.stop().await;
            }
        }

        #[tokio::test]
        async fn spawn_malformed_type_result_leaves_split_and_reports_typing_failure() {
            let (logs, _, _guard) = spawn_logs();
            let (mut rig, mut peer) = Rig::start(1, &METHODS).await;
            let work = action(&rig, Some("%7"), atm_tmux::PaneDirection::Below);
            prepare(
                &mut peer,
                Some("%7"),
                json!({"target_pane":"%42","cwd":null}),
            )
            .await;
            let (id, _) = request(&mut peer, METHODS[1]).await;
            peer.send(DaemonMessage::response_ok(id, json!({"pane":"%99"})))
                .await;
            let (id, _) = request(&mut peer, METHODS[2]).await;
            peer.send(DaemonMessage::response_ok(
                id,
                json!({"pane":"%unexpected"}),
            ))
            .await;
            guarded(work).await.unwrap();
            assert!(
                logs.text().contains("Failed to send command to pane"),
                "{}",
                logs.text()
            );
            assert!(logs.text().contains("expected null"));
            assert!(logs.text().contains("Spawned new agent"));
            sentinel(&rig, &mut peer).await;
            rig.stop().await;
        }

        #[tokio::test]
        async fn spawn_stage_failures_preserve_warning_phase_and_stop_without_replay() {
            for (stage, message, warn) in [
                (0, "foreign server", "Failed to spawn agent"),
                (1, "no space for pane", "Failed to spawn agent"),
                (
                    2,
                    "Failed to send command to new pane",
                    "Failed to send command to pane",
                ),
                (
                    2,
                    "Failed to send Enter to new pane",
                    "Failed to send Enter to pane",
                ),
            ] {
                let (logs, _guard) = Logs::capture();
                let (mut rig, mut peer) = Rig::start(1, &METHODS).await;
                let work = action(&rig, Some("%7"), atm_tmux::PaneDirection::Below);
                for (preceding, method) in METHODS.iter().take(stage).enumerate() {
                    let (id, _) = request(&mut peer, method).await;
                    peer.send(DaemonMessage::response_ok(
                        id,
                        if preceding == 0 {
                            json!({"target_pane":"%42","cwd":null})
                        } else {
                            json!({"pane":"%99"})
                        },
                    ))
                    .await;
                }
                let (id, _) = request(&mut peer, METHODS[stage]).await;
                peer.send(DaemonMessage::response_err(id, "refused", message))
                    .await;
                guarded(work).await.unwrap();
                assert!(logs.text().contains(warn), "{}", logs.text());
                assert!(logs.text().contains("refused"));
                sentinel(&rig, &mut peer).await;
                rig.stop().await;
            }
        }

        #[tokio::test]
        async fn spawn_capability_gate_at_every_stage_preserves_discovery() {
            for (minor, missing) in [(0, 0), (1, 0), (1, 1), (1, 2)] {
                let methods: Vec<_> = METHODS
                    .iter()
                    .enumerate()
                    .filter(|(n, _)| *n != missing)
                    .map(|(_, m)| *m)
                    .collect();
                let (logs, _guard) = Logs::capture();
                let (mut rig, mut peer) = Rig::start(minor, &methods).await;
                let work = action(&rig, Some("%7"), atm_tmux::PaneDirection::Below);
                for (stage, method) in METHODS.iter().take(missing).enumerate() {
                    let (id, _) = request(&mut peer, method).await;
                    peer.send(DaemonMessage::response_ok(
                        id,
                        if stage == 0 {
                            json!({"target_pane":"%42","cwd":null})
                        } else {
                            json!({"pane":"%99"})
                        },
                    ))
                    .await;
                }
                guarded(work).await.unwrap();
                assert!(logs.text().contains("does not support"), "{}", logs.text());
                sentinel(&rig, &mut peer).await;
                rig.stop().await;
            }
        }

        #[tokio::test]
        async fn spawn_cancel_closed_and_disconnect_never_replay() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let cancel = CancellationToken::new();
            cancel.cancel();
            guarded(spawn_agent(
                ControlHandle::new(tx),
                caller(),
                Some("%7".into()),
                None,
                atm_tmux::PaneDirection::Below,
                cancel,
                spawn_harness_reader(),
            ))
            .await
            .unwrap();
            assert!(rx.try_recv().is_err());
            let (tx, rx) = mpsc::unbounded_channel();
            drop(rx);
            let (logs, _guard) = Logs::capture();
            guarded(spawn_agent(
                ControlHandle::new(tx),
                caller(),
                None,
                None,
                atm_tmux::PaneDirection::Below,
                CancellationToken::new(),
                spawn_harness_reader(),
            ))
            .await
            .unwrap();
            assert!(
                logs.text().contains("control channel closed"),
                "{}",
                logs.text()
            );
            for stage in 0..3 {
                let (mut rig, mut peer) = Rig::start(1, &METHODS).await;
                let work = action(&rig, Some("%7"), atm_tmux::PaneDirection::Below);
                for (n, method) in METHODS.iter().take(stage + 1).enumerate() {
                    let (id, _) = request(&mut peer, method).await;
                    if n < stage {
                        peer.send(DaemonMessage::response_ok(
                            id,
                            if n == 0 {
                                json!({"target_pane":"%42","cwd":null})
                            } else {
                                json!({"pane":"%99"})
                            },
                        ))
                        .await;
                    }
                }
                drop(peer);
                guarded(work).await.unwrap();
                loop {
                    match guarded(rig.events.recv()).await.unwrap() {
                        Event::Control(_) => {}
                        Event::DaemonDisconnected(_) => break,
                        other => panic!("disconnect {other:?}"),
                    }
                }
                let mut peer = rig.connect(1, &METHODS).await;
                sentinel(&rig, &mut peer).await;
                rig.stop().await;
            }
        }

        #[tokio::test]
        async fn spawn_late_cancelled_stage_cannot_advance_a_fresh_action() {
            for (stage, method) in METHODS.iter().enumerate() {
                let (mut rig, mut peer) = Rig::start(1, &METHODS).await;
                let cancel = CancellationToken::new();
                let work = spawn_agent(
                    rig.control(),
                    caller(),
                    Some("%7".into()),
                    None,
                    atm_tmux::PaneDirection::Below,
                    cancel.clone(),
                    spawn_harness_reader(),
                );
                for (n, method) in METHODS.iter().take(stage).enumerate() {
                    let (id, _) = request(&mut peer, method).await;
                    peer.send(DaemonMessage::response_ok(
                        id,
                        if n == 0 {
                            json!({"target_pane":"%42","cwd":null})
                        } else {
                            json!({"pane":"%99"})
                        },
                    ))
                    .await;
                }
                let (old, _) = request(&mut peer, method).await;
                cancel.cancel();
                guarded(work).await.unwrap();
                let fresh = action(&rig, Some("%7"), atm_tmux::PaneDirection::Below);
                let (new, _) = request(&mut peer, METHODS[0]).await;
                assert_ne!(new, old);
                peer.send(DaemonMessage::response_ok(
                    old,
                    match stage {
                        0 => json!({"target_pane":"%42","cwd":null}),
                        1 => json!({"pane":"%99"}),
                        _ => Value::Null,
                    },
                ))
                .await;
                sentinel(&rig, &mut peer).await;
                peer.send(DaemonMessage::response_err(new, "refused", "fresh refusal"))
                    .await;
                guarded(fresh).await.unwrap();
                sentinel(&rig, &mut peer).await;
                rig.stop().await;
            }
        }

        // Break caught: command construction is outside the budget/cancellation poll boundary.
        #[tokio::test(start_paused = true)]
        async fn spawn_config_wait_ends_on_budget_or_cancel_without_splitting() {
            for timed_out in [false, true] {
                let (requests, mut received) = mpsc::unbounded_channel();
                let (entered, started) = tokio::sync::oneshot::channel();
                let (release, held) = tokio::sync::oneshot::channel::<String>();
                let cancel = CancellationToken::new();
                let stopped = cancel.clone();
                let work = tokio::spawn(async move {
                    let mut spawned = None;
                    let mut split_requested = false;
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
                    let flow = spawn_agent_sequence(
                        Some("%7".into()),
                        None,
                        atm_tmux::PaneDirection::Below,
                        deadline,
                        &mut spawned,
                        &mut split_requested,
                        move |method, _, _| {
                            requests.send(method).unwrap();
                            std::future::ready(Ok(json!({"target_pane":"%42","cwd":null})))
                        },
                        move |_| async move {
                            entered.send(()).unwrap();
                            Ok(held.await.unwrap())
                        },
                    );
                    tokio::select! {
                        biased;
                        () = stopped.cancelled() => {}
                        result = tokio::time::timeout_at(deadline, flow) => assert!(result.is_err()),
                    }
                    (spawned, split_requested)
                });
                assert_eq!(received.recv().await.unwrap(), METHODS[0]);
                started.await.unwrap();
                if timed_out {
                    tokio::time::advance(Duration::from_secs(40)).await;
                } else {
                    cancel.cancel();
                }
                assert_eq!(work.await.unwrap(), (None, false));
                assert!(
                    release.is_closed(),
                    "config completion must not resume a cancelled sequence"
                );
                assert!(
                    received.try_recv().is_err(),
                    "no split after cancelled/expired config build"
                );
            }
        }

        // Run isolated so every detached OS thread is reaped with its child process, even on panic.
        #[tokio::test]
        async fn spawn_config_reader_retains_one_read_across_cancelled_waits() {
            if std::env::var_os("ATM_SPAWN_READER_CHILD").is_none() {
                let output = guarded(tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "tui::kill_action_tests::spawn_request_tests::spawn_config_reader_retains_one_read_across_cancelled_waits", "--nocapture"])
                    .env("ATM_SPAWN_READER_CHILD", "1").kill_on_drop(true).output()).await.unwrap();
                assert!(
                    output.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }
            use std::sync::{
                atomic::{AtomicUsize, Ordering::SeqCst},
                Arc, Condvar, Mutex,
            };
            struct Load {
                admissions: Arc<AtomicUsize>,
                gate: Arc<(Mutex<bool>, Condvar)>,
                entered: mpsc::UnboundedSender<()>,
            }
            impl Clone for Load {
                fn clone(&self) -> Self {
                    self.admissions.fetch_add(1, SeqCst);
                    Self {
                        admissions: self.admissions.clone(),
                        gate: self.gate.clone(),
                        entered: self.entered.clone(),
                    }
                }
            }
            impl Load {
                fn read(&self) -> super::super::super::spawn::SpawnHarnessDefinition {
                    self.entered.send(()).unwrap();
                    drop(
                        self.gate
                            .1
                            .wait_while(self.gate.0.lock().unwrap(), |open| !*open)
                            .unwrap(),
                    );
                    super::super::super::spawn::default_spawn_harness()
                }
            }
            struct Release(Arc<(Mutex<bool>, Condvar)>);
            impl Drop for Release {
                fn drop(&mut self) {
                    *self.0 .0.lock().unwrap() = true;
                    self.0 .1.notify_all();
                }
            }
            let admissions = Arc::new(AtomicUsize::new(0));
            let gate = Arc::new((Mutex::new(false), Condvar::new()));
            let release = Release(gate.clone());
            let (entered, mut started) = mpsc::unbounded_channel();
            let load = Load {
                admissions: admissions.clone(),
                gate,
                entered,
            };
            let mut reader = SpawnHarnessReader {
                load: move || load.read(),
                pending: None,
            };
            for _ in 0..4 {
                let read = reader.read();
                tokio::pin!(read);
                let first = std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(std::future::Future::poll(read.as_mut(), cx))
                })
                .await;
                assert!(first.is_pending());
            }
            guarded(started.recv()).await.unwrap();
            assert_eq!(
                admissions.load(SeqCst),
                1,
                "each admitted thread synchronously clones its loader"
            );
            drop(release);
            let harness = guarded(reader.read()).await.unwrap();
            let command = super::super::super::spawn::build_spawn_command_for_harness(
                &harness,
                Some("/new target"),
                None,
            );
            assert!(command.starts_with("cd '/new target' && "), "{command}");
            assert_eq!(admissions.load(SeqCst), 1);
            assert!(reader.pending.is_none());
            let (send, receive) = tokio::sync::oneshot::channel();
            reader.pending = Some(receive);
            drop(send);
            assert!(
                guarded(reader.read()).await.is_err(),
                "worker loss is a value"
            );
            assert!(reader.pending.is_none());
        }

        #[tokio::test(start_paused = true)]
        async fn spawn_absolute_budget_is_shared_by_all_three_stages() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
            let work = tokio::spawn(async move {
                let mut spawned = None;
                let mut split_requested = false;
                let flow = spawn_agent_sequence(
                    Some("%7".into()),
                    None,
                    atm_tmux::PaneDirection::Below,
                    deadline,
                    &mut spawned,
                    &mut split_requested,
                    move |method, params, budget| {
                        let (reply, result) =
                            tokio::sync::oneshot::channel::<anyhow::Result<Value>>();
                        tx.send((method, params, budget, reply)).unwrap();
                        async move { result.await.unwrap() }
                    },
                    |_| std::future::ready(Ok("inert-agent".into())),
                );
                let result = tokio::time::timeout_at(deadline, flow).await;
                (result, spawned)
            });
            let (method, _, budget, reply) = rx.recv().await.unwrap();
            assert_eq!(method, METHODS[0]);
            assert_eq!(budget, Duration::from_secs(40));
            tokio::time::advance(Duration::from_secs(25)).await;
            reply
                .send(Ok(json!({"target_pane":"%42","cwd":null})))
                .unwrap();
            let (method, _, budget, reply) = rx.recv().await.unwrap();
            assert_eq!(method, METHODS[1]);
            assert_eq!(budget, Duration::from_secs(15));
            tokio::time::advance(Duration::from_secs(10)).await;
            reply.send(Ok(json!({"pane":"%99"}))).unwrap();
            let (method, _, budget, reply) = rx.recv().await.unwrap();
            assert_eq!(method, METHODS[2]);
            assert_eq!(budget, Duration::from_secs(5));
            tokio::time::advance(Duration::from_secs(5)).await;
            let (result, spawned) = work.await.unwrap();
            assert!(result.is_err());
            assert_eq!(spawned.as_deref(), Some("%99"));
            assert!(reply.is_closed());
            assert!(rx.try_recv().is_err());
        }

        #[tokio::test(start_paused = true)]
        async fn spawn_offline_timeout_or_cancel_is_not_admitted_later() {
            for timed_out in [false, true] {
                let (tx, mut rx) = mpsc::unbounded_channel();
                let cancel = CancellationToken::new();
                let work = spawn_agent(
                    ControlHandle::new(tx),
                    caller(),
                    Some("%7".into()),
                    None,
                    atm_tmux::PaneDirection::Below,
                    cancel.clone(),
                    spawn_harness_reader(),
                );
                let queued = rx.recv().await.unwrap();
                if timed_out {
                    tokio::time::advance(Duration::from_secs(40)).await;
                } else {
                    cancel.cancel();
                }
                guarded(work).await.unwrap();
                assert!(rx.try_recv().is_err());
                tokio::time::resume();
                let (mut rig, mut peer) = Rig::start(1, &METHODS).await;
                rig.commands.send(queued).unwrap();
                sentinel(&rig, &mut peer).await;
                rig.stop().await;
                tokio::time::pause();
            }
        }

        #[derive(Clone)]
        struct SpawnLog {
            bytes: Logs,
            finished: std::sync::Arc<tokio::sync::Notify>,
        }
        impl std::io::Write for SpawnLog {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let count = std::io::Write::write(&mut self.bytes, bytes)?;
                if self.bytes.text().contains("Spawned new agent") {
                    self.finished.notify_one();
                }
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        fn spawn_logs() -> (
            Logs,
            std::sync::Arc<tokio::sync::Notify>,
            tracing::subscriber::DefaultGuard,
        ) {
            let bytes = Logs::default();
            let finished = std::sync::Arc::new(tokio::sync::Notify::new());
            let writer = SpawnLog {
                bytes: bytes.clone(),
                finished: finished.clone(),
            };
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_max_level(tracing::Level::INFO)
                .with_writer(move || writer.clone())
                .finish();
            (
                bytes,
                finished,
                tracing::subscriber::set_default(subscriber),
            )
        }

        #[tokio::test]
        async fn spawn_render_error_cancels_owned_worker_without_waiting_for_deadline() {
            if std::env::var_os("ATM_SPAWN_RENDER_CHILD").is_none() {
                let dir = tempfile::tempdir().unwrap();
                let result = guarded(tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "tui::kill_action_tests::spawn_request_tests::spawn_render_error_cancels_owned_worker_without_waiting_for_deadline", "--nocapture"])
                    .env("ATM_SPAWN_RENDER_CHILD", "1")
                    .env("TMUX", format!("{}/private,1,0", dir.path().display()))
                    .env("TMUX_PANE", "%7").env("TMUX_TMPDIR", dir.path())
                    .env("ATM_TMUX_SOCKET", "spawn-render-proof").env_remove("ATM_AGENT")
                    .output()).await.unwrap();
                assert!(
                    result.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
                return;
            }
            use std::os::fd::AsRawFd;
            let saved_stdout = unsafe { libc::dup(libc::STDOUT_FILENO) };
            assert!(saved_stdout >= 0);
            let full = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap();
            let mut terminal = Terminal::with_options(
                CrosstermBackend::new(io::stdout()),
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
                },
            )
            .unwrap();
            let (commands, mut requests) = mpsc::unbounded_channel();
            let control = ControlHandle::new(commands.clone());
            let (input, mut events) = mpsc::unbounded_channel();
            let cancel = CancellationToken::new();
            let mut app = selected_app(Some("%42"));
            let (capture, _) = tokio::sync::watch::channel(None);
            let (beads, _) = tokio::sync::watch::channel(None);
            let key = |c| {
                Event::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Char(c),
                    KeyModifiers::NONE,
                ))
            };
            input.send(key('o')).unwrap();
            input.send(key('j')).unwrap();
            // Only in-memory channels await under the paused clock; stdio writes
            // are synchronous and the parent's process I/O clock is not paused.
            tokio::time::pause();
            let script = async {
                let queued = [
                    requests.recv().await.unwrap(),
                    requests.recv().await.unwrap(),
                ];
                let methods: HashSet<_> = queued
                    .iter()
                    .map(|command| {
                        let ClientCommand::Control(request) = command else {
                            panic!("expected a pending control request")
                        };
                        // ControlCommand's fields are private to atm_tui; its
                        // derived Debug exposes the actual method without a new API.
                        let request = format!("{request:?}");
                        request
                            .split_once("method: \"")
                            .and_then(|(_, rest)| rest.split_once('"'))
                            .map(|(method, _)| method.to_owned())
                            .expect("pending request has a method")
                    })
                    .collect();
                assert_eq!(
                    methods,
                    HashSet::from([
                        "org.tree".to_owned(),
                        "session.spawn_prepare_largest".to_owned()
                    ])
                );
                assert_eq!(
                    unsafe { libc::dup2(full.as_raw_fd(), libc::STDOUT_FILENO) },
                    libc::STDOUT_FILENO
                );
                input.send(key('?')).unwrap();
                queued
            };
            let (result, queued) = tokio::join!(
                run_event_loop(
                    &mut terminal,
                    &mut app,
                    &mut events,
                    &commands,
                    &control,
                    &cancel,
                    &capture,
                    &beads
                ),
                script
            );
            assert_eq!(
                unsafe { libc::dup2(saved_stdout, libc::STDOUT_FILENO) },
                libc::STDOUT_FILENO
            );
            unsafe {
                libc::close(saved_stdout);
            }
            assert!(result.is_err(), "render failure must escape the event loop");
            let stopped_at = tokio::time::Instant::now();
            drop(control);
            drop(commands);
            // tui_decisions.rs: DecisionController::drop queues Unwatch before this return.
            assert_eq!(
                format!("{:?}", requests.try_recv().unwrap()),
                "Control(ControlCommand { input: Unwatch, reply: None })"
            );
            // Workers hold the remaining senders. Keep both pending replies
            // alive: closure must come from cancellation, not response loss.
            assert!(requests.recv().await.is_none());
            assert_eq!(
                tokio::time::Instant::now(),
                stopped_at,
                "orphaned spawn lived until its 40-second deadline"
            );
            drop(queued);
        }

        #[tokio::test]
        async fn spawn_stall_keeps_input_live_and_owns_one_reusable_slot() {
            if std::env::var_os("ATM_SPAWN_LOOP_CHILD").is_none() {
                let dir = tempfile::tempdir().unwrap();
                let result=guarded(tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact","tui::kill_action_tests::spawn_request_tests::spawn_stall_keeps_input_live_and_owns_one_reusable_slot","--nocapture"])
                    .env("ATM_SPAWN_LOOP_CHILD","1").env("TMUX",format!("{}/private,1,0",dir.path().display())).env("TMUX_PANE","%7")
                    .env("TMUX_TMPDIR",dir.path()).env("ATM_TMUX_SOCKET","spawn-loop-proof").env("ATM_SPAWN_BIN","/bin/false").env_remove("ATM_AGENT")
                    .output()).await.unwrap();
                assert!(
                    result.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
                return;
            }
            for held_stage in 0..3 {
                let (logs, finished, _guard) = spawn_logs();
                let (mut rig, mut peer) = Rig::start(1, &METHODS).await;
                let mut terminal = Terminal::with_options(
                    CrosstermBackend::new(io::stdout()),
                    ratatui::TerminalOptions {
                        viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(
                            0, 0, 80, 24,
                        )),
                    },
                )
                .unwrap();
                let mut app = selected_app(Some("%42"));
                let (capture, _) = tokio::sync::watch::channel(None);
                let (beads, _) = tokio::sync::watch::channel(None);
                let control = rig.control();
                let input = rig.event_tx.clone();
                let key = |c| {
                    Event::Key(crossterm::event::KeyEvent::new(
                        KeyCode::Char(c),
                        KeyModifiers::NONE,
                    ))
                };
                input.send(key('o')).unwrap();
                input.send(key('j')).unwrap();
                let script = async {
                    // Complete one sequence and observe its final log event;
                    // then a second keypress must reuse the finished owned slot.
                    for (stage, expected) in METHODS.iter().enumerate() {
                        let MessageType::Request { id, method, .. } = peer.recv().await else {
                            panic!("first spawn sequence")
                        };
                        assert_eq!(method, *expected);
                        peer.send(DaemonMessage::response_ok(
                            id,
                            match stage {
                                0 => json!({"target_pane":"%42","cwd":null}),
                                1 => json!({"pane":"%99"}),
                                _ => Value::Null,
                            },
                        ))
                        .await;
                    }
                    finished.notified().await;
                    input.send(key('o')).unwrap();
                    input.send(key('j')).unwrap();
                    for (stage, expected) in METHODS.iter().take(held_stage + 1).enumerate() {
                        let MessageType::Request {
                            id, method, params, ..
                        } = peer.recv().await
                        else {
                            panic!("staged spawn")
                        };
                        assert_eq!(method, *expected);
                        assert!(params["caller_server"].as_str().unwrap().starts_with('/'));
                        if stage == 0 {
                            assert_eq!(params["caller_pane"], "%7");
                        } else if stage == 1 {
                            assert_eq!(
                                params["cwd"],
                                std::env::current_dir().unwrap().to_str().unwrap()
                            );
                        }
                        if stage < held_stage {
                            peer.send(DaemonMessage::response_ok(
                                id,
                                if stage == 0 {
                                    json!({"target_pane":"%42","cwd":null})
                                } else {
                                    json!({"pane":"%99"})
                                },
                            ))
                            .await;
                        }
                    }
                    input.send(key('o')).unwrap();
                    input.send(key('l')).unwrap();
                    input.send(key('k')).unwrap();
                    input.send(key('r')).unwrap();
                    assert!(matches!(peer.recv().await, MessageType::Discover));
                    input.send(key('q')).unwrap();
                };
                let (result, ()) = guarded(async {
                    tokio::join!(
                        run_event_loop(
                            &mut terminal,
                            &mut app,
                            &mut rig.events,
                            &rig.commands,
                            &control,
                            &rig.cancel,
                            &capture,
                            &beads
                        ),
                        script
                    )
                })
                .await;
                result.unwrap();
                assert!(app.should_quit);
                assert!(app.selected_session().is_none());
                assert_eq!(
                    logs.text()
                        .matches("Spawn request already in flight")
                        .count(),
                    1,
                    "{}",
                    logs.text()
                );
                rig.stop().await;
            }
        }
    }

    mod capture_request_tests {
        use super::*;

        // A client-side capture must never publish before daemon admission.
        #[tokio::test]
        async fn preview_uses_daemon_before_publishing() {
            let (mut rig, mut peer) = Rig::start(1, &["session.capture"]).await;
            let (events, mut captured) = mpsc::unbounded_channel();
            let (_target, target_rx) = tokio::sync::watch::channel(Some("%42".into()));
            let task = spawn_capture_task(rig.control(), events, rig.cancel.clone(), target_rx);
            tokio::select! {
                event = captured.recv() => panic!("preview bypassed session.capture: {event:?}"),
                message = peer.recv() => assert!(matches!(message, MessageType::Request { method, .. } if method == "session.capture")),
            }
            task.abort();
            rig.stop().await;
        }

        async fn capture(peer: &mut Peer, target: &str, context: Option<CallerContext>) -> String {
            let MessageType::Request {
                id,
                method,
                params,
                from_pane,
            } = peer.recv().await
            else {
                panic!("capture request")
            };
            assert_eq!(method, "session.capture");
            assert_eq!(params.get("target"), Some(&serde_json::json!(target)));
            assert_eq!(params.get("tail"), Some(&serde_json::Value::Null));
            assert_eq!(params.get("prompt"), Some(&serde_json::json!(false)));
            assert_eq!(params.as_object().unwrap().len(), 4);
            if let Some(context) = context {
                assert_eq!(
                    params.get("caller_server"),
                    Some(&serde_json::json!(context.caller_server))
                );
                assert_eq!(from_pane, context.from_pane);
            } else {
                assert!(params
                    .get("caller_server")
                    .and_then(|v| v.as_str())
                    .is_some_and(|path| std::path::Path::new(path).is_absolute()));
            }
            id
        }

        #[tokio::test]
        async fn preview_capture_binds_the_exact_target_and_preserves_lines_including_empty() {
            let (mut rig, mut peer) = Rig::start(1, &["session.capture"]).await;
            for lines in [vec!["first", "", "last café λ"], vec![]] {
                let control = rig.control();
                let request =
                    tokio::spawn(
                        async move { capture_preview(&control, "%42".into(), caller()).await },
                    );
                let id = capture(&mut peer, "%42", Some(caller())).await;
                peer.send(DaemonMessage::response_ok(
                    id,
                    serde_json::json!({"lines": lines}),
                ))
                .await;
                assert!(
                    matches!(guarded(request).await.unwrap(), Some(Event::CaptureUpdate { pane_id, lines: actual })
                    if pane_id == "%42" && actual == lines)
                );
                rig.commands.send(ClientCommand::Discover).unwrap();
                assert!(matches!(peer.recv().await, MessageType::Discover));
            }
            rig.stop().await;
        }

        // Missing/wrongly typed payloads must not erase a last-good preview or
        // turn an error into a successful empty capture. No local retry/fallback.
        #[tokio::test]
        async fn preview_capture_errors_and_malformed_replies_keep_last_good_without_retry() {
            let (logs, _guard) = Logs::capture();
            let (mut rig, mut peer) = Rig::start(1, &["session.capture"]).await;
            let mut app = selected_app(Some("%42"));
            app.capture_pane_id = Some("%42".into());
            app.captured_output = vec!["last good".into()];
            for payload in [
                None,
                Some(serde_json::json!({})),
                Some(serde_json::json!({"lines": null})),
                Some(serde_json::json!({"lines": ["one", 2]})),
            ] {
                let control = rig.control();
                let request =
                    tokio::spawn(
                        async move { capture_preview(&control, "%42".into(), caller()).await },
                    );
                let id = capture(&mut peer, "%42", Some(caller())).await;
                let response = match payload {
                    Some(value) => DaemonMessage::response_ok(id, value),
                    None => DaemonMessage::response_err(id, "refused", "wrong server"),
                };
                peer.send(response).await;
                if let Some(Event::CaptureUpdate { pane_id, lines }) =
                    guarded(request).await.unwrap()
                {
                    app.update_capture(&pane_id, lines);
                    panic!("invalid capture was published");
                }
                assert_eq!(app.captured_output, ["last good"]);
                rig.commands.send(ClientCommand::Discover).unwrap();
                assert!(matches!(peer.recv().await, MessageType::Discover));
            }
            assert!(
                logs.text().contains("wrong server") && logs.text().contains("refused"),
                "{}",
                logs.text()
            );
            rig.stop().await;
        }

        #[tokio::test]
        async fn a_target_changed_during_a_capture_is_captured_right_after_it_and_stale_reply_is_ignored(
        ) {
            let (mut rig, mut peer) = Rig::start(1, &["session.capture"]).await;
            let (events, mut captured) = mpsc::unbounded_channel();
            let (target, target_rx) = tokio::sync::watch::channel(None);
            let mut app = selected_app(Some("%42"));
            sync_capture_target(&mut app, &target);
            let task = spawn_capture_task(rig.control(), events, rig.cancel.clone(), target_rx);
            let old = capture(&mut peer, "%42", None).await;
            app.update_sessions(vec![atm_core::SessionView {
                id: "selected".into(),
                project_root: Some("/project".into()),
                tmux_pane: Some("%43".into()),
                ..Default::default()
            }]);
            sync_capture_target(&mut app, &target);
            peer.send(DaemonMessage::response_ok(
                old,
                serde_json::json!({"lines": ["old selection"]}),
            ))
            .await;
            let Some(Event::CaptureUpdate { pane_id, lines }) = guarded(captured.recv()).await
            else {
                panic!("old capture")
            };
            assert_eq!(pane_id, "%42");
            app.update_capture(&pane_id, lines);
            assert!(app.captured_output.is_empty());
            let next = capture(&mut peer, "%43", None).await;
            peer.send(DaemonMessage::response_ok(
                next,
                serde_json::json!({"lines": ["current selection"]}),
            ))
            .await;
            let Some(Event::CaptureUpdate { pane_id, lines }) = guarded(captured.recv()).await
            else {
                panic!("new capture")
            };
            app.update_capture(&pane_id, lines);
            assert_eq!(app.captured_output, ["current selection"]);
            rig.cancel.cancel();
            guarded(task).await.unwrap();
            rig.stop().await;
        }

        #[tokio::test]
        async fn a_stale_reply_after_filter_hides_the_selection_cannot_restore_its_preview() {
            let (mut rig, mut peer) = Rig::start(1, &["session.capture"]).await;
            let (events, mut captured) = mpsc::unbounded_channel();
            let (target, target_rx) = tokio::sync::watch::channel(None);
            let mut app = selected_app(Some("%42"));
            app.tmux_session_filter = Some("private".into());
            app.update_filter_panes(["%42".into()].into_iter().collect());
            sync_capture_target(&mut app, &target);
            let task = spawn_capture_task(rig.control(), events, rig.cancel.clone(), target_rx);
            let old = capture(&mut peer, "%42", None).await;
            app.update_filter_panes(Default::default());
            sync_capture_target(&mut app, &target);
            assert!(app.selected_session().is_none() && app.capture_pane_id.is_none());
            peer.send(DaemonMessage::response_ok(
                old,
                serde_json::json!({"lines": ["hidden selection"]}),
            ))
            .await;
            let Some(Event::CaptureUpdate { pane_id, lines }) = guarded(captured.recv()).await
            else {
                panic!("old capture")
            };
            app.update_capture(&pane_id, lines);
            assert!(app.captured_output.is_empty());
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
            rig.cancel.cancel();
            guarded(task).await.unwrap();
            rig.stop().await;
        }

        #[tokio::test]
        async fn a_closed_target_drains_successful_in_flight_and_latest_captures_serially() {
            let (mut rig, mut peer) = Rig::start(1, &["session.capture"]).await;
            let (events, mut captured) = mpsc::unbounded_channel();
            let (target, target_rx) = tokio::sync::watch::channel(Some("%42".into()));
            let task = spawn_capture_task(rig.control(), events, rig.cancel.clone(), target_rx);
            let first = capture(&mut peer, "%42", None).await;
            target.send_replace(Some("%43".into()));
            drop(target);
            peer.send(DaemonMessage::response_ok(
                first,
                serde_json::json!({"lines": ["old"]}),
            ))
            .await;
            let second = capture(&mut peer, "%43", None).await;
            peer.send(DaemonMessage::response_ok(
                second,
                serde_json::json!({"lines": ["new"]}),
            ))
            .await;
            guarded(task).await.unwrap();
            for expected in ["%42", "%43"] {
                assert!(
                    matches!(captured.recv().await, Some(Event::CaptureUpdate { pane_id, .. }) if pane_id == expected)
                );
            }
            assert!(captured.recv().await.is_none());
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
            rig.stop().await;
        }

        #[tokio::test]
        async fn preview_capture_capability_and_disconnect_failures_publish_nothing() {
            for (minor, methods) in [
                (0, vec!["session.capture"]),
                (1, vec![]),
                (1, vec!["session.kill"]),
            ] {
                let (mut rig, mut peer) = Rig::start(minor, &methods).await;
                assert!(capture_preview(&rig.control(), "%42".into(), caller())
                    .await
                    .is_none());
                rig.commands.send(ClientCommand::Discover).unwrap();
                assert!(matches!(peer.recv().await, MessageType::Discover));
                rig.stop().await;
            }
            let (mut rig, mut peer) = Rig::start(1, &["session.capture"]).await;
            let control = rig.control();
            let request =
                tokio::spawn(
                    async move { capture_preview(&control, "%42".into(), caller()).await },
                );
            let _ = capture(&mut peer, "%42", Some(caller())).await;
            drop(peer);
            assert!(guarded(request).await.unwrap().is_none());
            rig.stop().await;
        }

        // The daemon never releases its reply. Wire receipt gates the keys,
        // Discover proves the render loop progresses, and quit ends the worker.
        #[tokio::test]
        async fn capture_stall_keeps_navigation_discover_and_quit_live() {
            let (mut rig, mut peer) = Rig::start(1, &["session.capture"]).await;
            let mut terminal = Terminal::with_options(
                CrosstermBackend::new(io::stdout()),
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
                },
            )
            .unwrap();
            let mut app = selected_app(Some("%42"));
            let (target, target_rx) = tokio::sync::watch::channel(None);
            let (beads, _beads_rx) = tokio::sync::watch::channel(None);
            let control = rig.control();
            let task = spawn_capture_task(
                control.clone(),
                rig.event_tx.clone(),
                rig.cancel.clone(),
                target_rx,
            );
            let input = rig.event_tx.clone();
            let script = async {
                let _ = capture(&mut peer, "%42", None).await;
                for key in ['k', 'r'] {
                    input
                        .send(Event::Key(crossterm::event::KeyEvent::new(
                            KeyCode::Char(key),
                            KeyModifiers::NONE,
                        )))
                        .unwrap();
                }
                assert!(matches!(peer.recv().await, MessageType::Discover));
                input
                    .send(Event::Key(crossterm::event::KeyEvent::new(
                        KeyCode::Char('q'),
                        KeyModifiers::NONE,
                    )))
                    .unwrap();
            };
            let (result, ()) = guarded(async {
                tokio::join!(
                    run_event_loop(
                        &mut terminal,
                        &mut app,
                        &mut rig.events,
                        &rig.commands,
                        &control,
                        &rig.cancel,
                        &target,
                        &beads
                    ),
                    script
                )
            })
            .await;
            result.unwrap();
            assert!(app.should_quit && app.selected_session().is_none());
            rig.cancel.cancel();
            guarded(task).await.unwrap();
            rig.stop().await;
        }
    }

    // Reuse the subscribed-peer rig without changing the inherited kill proofs.
    mod interrupt_request_tests {
        use super::*;

        async fn interrupt(peer: &mut Peer) -> String {
            let MessageType::Request {
                id,
                method,
                params,
                from_pane,
            } = peer.recv().await
            else {
                panic!("interrupt request")
            };
            assert_eq!(method, "session.interrupt");
            assert_eq!(
                params,
                serde_json::json!({"target": "%42", "caller_server": "/private/caller-server"})
            );
            assert_eq!(from_pane.as_deref(), Some("%7"));
            id
        }

        // I-E's exact-target assertion moves from the tmux boundary to the wire.
        #[tokio::test]
        async fn interrupt_snapshots_the_selected_pane_before_spawning() {
            let (mut rig, mut peer) = Rig::start(1, &["session.interrupt"]).await;
            let mut app = selected_app(Some("%42"));
            let mut action =
                spawn_interrupt_selected_pane(&app, rig.control(), caller(), rig.cancel.clone())
                    .unwrap();
            app.replace_sessions(vec![]);
            let id = tokio::select! {
                result = &mut action => panic!("interrupt completed without a daemon request: {result:?}"),
                id = interrupt(&mut peer) => id,
            };
            peer.send(DaemonMessage::response_ok(
                id,
                serde_json::json!({"pane": "%42"}),
            ))
            .await;
            guarded(action).await.unwrap();
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
            rig.stop().await;
        }

        #[tokio::test]
        async fn interrupt_refusals_warn_without_retry_or_optimistic_changes() {
            let (logs, _guard) = Logs::capture();
            let (mut rig, mut peer) = Rig::start(1, &["session.interrupt"]).await;
            let app = selected_app(Some("%42"));
            for (code, message) in [
                ("refused", "agent is clearing"),
                ("unavailable", "agent is quarantined"),
                ("internal", "pane disappeared"),
            ] {
                let action = spawn_interrupt_selected_pane(
                    &app,
                    rig.control(),
                    caller(),
                    rig.cancel.clone(),
                )
                .unwrap();
                let id = interrupt(&mut peer).await;
                peer.send(DaemonMessage::response_err(id, code, message))
                    .await;
                guarded(action).await.unwrap();
                assert!(
                    logs.text().contains(code) && logs.text().contains(message),
                    "{}",
                    logs.text()
                );
                assert!(
                    app.selected_session().is_some(),
                    "no optimistic row removal"
                );
                rig.commands.send(ClientCommand::Discover).unwrap();
                assert!(matches!(peer.recv().await, MessageType::Discover));
            }
            rig.stop().await;
        }

        #[tokio::test]
        async fn interrupt_unsupported_and_closed_connections_warn_without_fallback() {
            let (logs, _guard) = Logs::capture();
            let (mut rig, mut peer) = Rig::start(1, &["session.kill"]).await;
            let app = selected_app(Some("%42"));
            guarded(
                spawn_interrupt_selected_pane(&app, rig.control(), caller(), rig.cancel.clone())
                    .unwrap(),
            )
            .await
            .unwrap();
            assert!(
                logs.text()
                    .contains("daemon does not support session.interrupt"),
                "{}",
                logs.text()
            );
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
            rig.stop().await;
            for close_reply in [false, true] {
                let (tx, mut rx) = mpsc::unbounded_channel();
                if !close_reply {
                    rx.close();
                }
                let action = spawn_interrupt_selected_pane(
                    &app,
                    ControlHandle::new(tx),
                    caller(),
                    CancellationToken::new(),
                )
                .unwrap();
                if close_reply {
                    drop(guarded(rx.recv()).await.unwrap());
                }
                guarded(action).await.unwrap();
            }
            assert_eq!(
                logs.text().matches("control channel closed").count(),
                2,
                "{}",
                logs.text()
            );
        }

        async fn abandoned_command_is_not_sent(command: ClientCommand) {
            let (mut rig, mut peer) = Rig::start(1, &["session.interrupt"]).await;
            rig.commands.send(command).unwrap();
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
            rig.stop().await;
        }

        #[tokio::test]
        async fn interrupt_already_cancelled_never_queues_a_request() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let cancel = CancellationToken::new();
            cancel.cancel();
            guarded(
                spawn_interrupt_selected_pane(
                    &selected_app(Some("%42")),
                    ControlHandle::new(tx),
                    caller(),
                    cancel,
                )
                .unwrap(),
            )
            .await
            .unwrap();
            assert!(rx.try_recv().is_err());
        }

        #[tokio::test(start_paused = true)]
        async fn interrupt_timeout_includes_offline_queue_and_never_sends_it_later() {
            let (logs, _guard) = Logs::capture();
            let (tx, mut rx) = mpsc::unbounded_channel();
            let action = spawn_interrupt_selected_pane(
                &selected_app(Some("%42")),
                ControlHandle::new(tx),
                caller(),
                CancellationToken::new(),
            )
            .unwrap();
            let command = rx.recv().await.unwrap();
            tokio::time::advance(Duration::from_secs(40)).await;
            guarded(action).await.unwrap();
            assert!(
                logs.text().contains("outcome may be unknown"),
                "{}",
                logs.text()
            );
            assert!(rx.try_recv().is_err(), "timeout queued a retry");
            tokio::time::resume(); // All paused-clock work above is in-memory.
            abandoned_command_is_not_sent(command).await;
        }

        #[tokio::test]
        async fn interrupt_cancelled_in_the_queue_is_not_sent_later() {
            let (logs, _guard) = Logs::capture();
            let (tx, mut rx) = mpsc::unbounded_channel();
            let cancel = CancellationToken::new();
            let action = spawn_interrupt_selected_pane(
                &selected_app(Some("%42")),
                ControlHandle::new(tx),
                caller(),
                cancel.clone(),
            )
            .unwrap();
            let command = guarded(rx.recv()).await.unwrap();
            cancel.cancel();
            guarded(action).await.unwrap();
            assert!(
                logs.text().contains("outcome may be unknown"),
                "{}",
                logs.text()
            );
            abandoned_command_is_not_sent(command).await;
        }

        #[tokio::test]
        async fn interrupt_cancelled_after_admission_does_not_retry() {
            let (logs, _guard) = Logs::capture();
            let (mut rig, mut peer) = Rig::start(1, &["session.interrupt"]).await;
            let cancel = CancellationToken::new();
            let action = spawn_interrupt_selected_pane(
                &selected_app(Some("%42")),
                rig.control(),
                caller(),
                cancel.clone(),
            )
            .unwrap();
            interrupt(&mut peer).await;
            cancel.cancel();
            guarded(action).await.unwrap();
            assert!(
                logs.text().contains("outcome may be unknown"),
                "{}",
                logs.text()
            );
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
            rig.stop().await;
        }

        // A held interrupt must not block input or queue repeated keypresses.
        // Wire receipt gates later keys: a spawned request may otherwise be
        // overtaken by an already-queued Discover before its first poll.
        #[tokio::test]
        async fn interrupt_stall_keeps_navigation_discover_and_quit_live() {
            let (logs, _guard) = Logs::capture();
            let (mut rig, mut peer) = Rig::start(1, &["session.interrupt"]).await;
            let mut terminal = Terminal::with_options(
                CrosstermBackend::new(io::stdout()),
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
                },
            )
            .unwrap();
            let mut app = selected_app(Some("%42"));
            let (capture, _capture_rx) = tokio::sync::watch::channel(None);
            let (beads, _beads_rx) = tokio::sync::watch::channel(None);
            let control = rig.control();
            let input = rig.event_tx.clone();
            let key = |c| {
                Event::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Char(c),
                    KeyModifiers::NONE,
                ))
            };
            input.send(key('I')).unwrap();
            let script = async {
                let message = peer.recv().await;
                let MessageType::Request {
                    method,
                    params,
                    from_pane,
                    ..
                } = message
                else {
                    panic!("expected daemon interrupt before Discover, got {message:?}")
                };
                assert_eq!(method, "session.interrupt");
                assert_eq!(params.get("target"), Some(&serde_json::json!("%42")));
                assert!(params
                    .get("caller_server")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| std::path::Path::new(s).is_absolute()));
                assert_eq!(
                    from_pane,
                    std::env::var("TMUX_PANE").ok().filter(|s| !s.is_empty())
                );
                input.send(key('I')).unwrap();
                input.send(key('k')).unwrap();
                input.send(key('r')).unwrap();
                assert!(matches!(peer.recv().await, MessageType::Discover));
                input.send(key('q')).unwrap();
                // No reply is released: quit must cancel/abort the owned worker.
            };
            let (result, ()) = guarded(async {
                tokio::join!(
                    run_event_loop(
                        &mut terminal,
                        &mut app,
                        &mut rig.events,
                        &rig.commands,
                        &control,
                        &rig.cancel,
                        &capture,
                        &beads,
                    ),
                    script
                )
            })
            .await;
            result.unwrap();
            assert_eq!(
                logs.text()
                    .matches("Interrupt request already in flight")
                    .count(),
                1,
                "{}",
                logs.text()
            );
            assert!(app.should_quit);
            assert!(
                app.selected_session().is_none(),
                "navigation must reach the group row"
            );
            rig.stop().await;
        }
    }

    mod filter_request_tests {
        use super::*;
        use std::{future::Future, pin::Pin, task::Poll};
        use tokio::time::{advance, Instant};

        async fn panes(peer: &mut Peer) -> String {
            let MessageType::Request {
                id,
                method,
                params,
                from_pane,
            } = peer.recv().await
            else {
                panic!("membership request")
            };
            assert_eq!(method, "session.panes");
            assert_eq!(
                params,
                serde_json::json!({"session_name":"work","caller_server":"/private/caller-server"})
            );
            assert_eq!(from_pane.as_deref(), Some("%7"));
            id
        }

        fn query(control: ControlHandle) -> tokio::task::JoinHandle<Option<HashSet<String>>> {
            tokio::spawn(async move { filter_session_panes(&control, "work", caller()).await })
        }

        // Breaks if malformed/error responses erase the last good set, if
        // empty success is ignored, or literal context is lost at admission.
        #[tokio::test]
        async fn filter_reply_matrix_preserves_last_good_and_empty_success_clears() {
            let (mut rig, mut peer) = Rig::start(1, &["session.panes"]).await;
            for (payload, expected) in [
                (
                    Some(serde_json::json!({"pane_ids":["%42","%42"]})),
                    Some(HashSet::from(["%42".into()])),
                ),
                (
                    Some(serde_json::json!({"pane_ids":[]})),
                    Some(HashSet::new()),
                ),
                (Some(serde_json::json!({})), None),
                (Some(serde_json::json!({"pane_ids":null})), None),
                (Some(serde_json::json!({"pane_ids":"%42"})), None),
                (Some(serde_json::json!({"pane_ids":["%42",7]})), None),
                (None, None),
            ] {
                let mut app = selected_app(Some("%42"));
                app.tmux_session_filter = Some("work".into());
                app.update_filter_panes(HashSet::from(["%42".into()]));
                let pending = query(rig.control());
                let id = panes(&mut peer).await;
                if let Some(payload) = payload {
                    peer.send(DaemonMessage::response_ok(id, payload)).await;
                } else {
                    peer.send(DaemonMessage::response_err(id, "refused", "foreign server"))
                        .await;
                }
                let result = guarded(pending).await.unwrap();
                assert_eq!(result, expected);
                if let Some(ids) = result {
                    app.update_filter_panes(ids);
                }
                assert_eq!(
                    app.filter_pane_ids,
                    expected.unwrap_or_else(|| HashSet::from(["%42".into()]))
                );
                rig.commands.send(ClientCommand::Discover).unwrap();
                assert!(matches!(peer.recv().await, MessageType::Discover));
            }
            rig.stop().await;
        }

        #[tokio::test]
        async fn filter_unsupported_and_closed_requests_publish_nothing() {
            let (mut rig, mut peer) = Rig::start(1, &["session.capture"]).await;
            assert!(guarded(query(rig.control())).await.unwrap().is_none());
            rig.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
            rig.stop().await;
            for close_reply in [false, true] {
                let (tx, mut rx) = mpsc::unbounded_channel();
                if !close_reply {
                    rx.close();
                }
                let pending = query(ControlHandle::new(tx));
                if close_reply {
                    drop(guarded(rx.recv()).await.unwrap());
                }
                assert!(guarded(pending).await.unwrap().is_none());
            }
        }

        struct MemoryRig {
            commands: mpsc::UnboundedReceiver<ClientCommand>,
            events: mpsc::UnboundedReceiver<Event>,
            contexts: mpsc::UnboundedReceiver<usize>,
            cancel: CancellationToken,
            task: tokio::task::JoinHandle<()>,
        }
        fn memory_rig(first_context_fails: bool) -> MemoryRig {
            let (tx, commands) = mpsc::unbounded_channel();
            let (event_tx, events) = mpsc::unbounded_channel();
            let (context_tx, contexts) = mpsc::unbounded_channel();
            let cancel = CancellationToken::new();
            let mut observations = 0;
            let task = spawn_filter_task(
                "work".into(),
                ControlHandle::new(tx),
                event_tx,
                cancel.clone(),
                move || {
                    observations += 1;
                    context_tx.send(observations).unwrap();
                    if first_context_fails && observations == 1 {
                        Err(super::super::super::rpc::RpcError::Connection(
                            "context unavailable".into(),
                        ))
                    } else {
                        Ok(caller())
                    }
                },
            );
            MemoryRig {
                commands,
                events,
                contexts,
                cancel,
                task,
            }
        }
        async fn stop(rig: MemoryRig) {
            rig.cancel.cancel();
            guarded(rig.task).await.unwrap();
        }

        // Breaks eager retry or resolving context once at task startup.
        #[tokio::test(start_paused = true)]
        async fn filter_polls_immediately_then_every_three_seconds_with_fresh_context() {
            let mut rig = memory_rig(false);
            let start = Instant::now();
            for (elapsed, observation) in [(0, 1), (3, 2), (6, 3)] {
                drop(rig.commands.recv().await.unwrap());
                assert_eq!(rig.contexts.recv().await, Some(observation));
                assert_eq!(Instant::now() - start, Duration::from_secs(elapsed));
                assert!(rig.events.try_recv().is_err());
            }
            stop(rig).await;
        }

        #[tokio::test(start_paused = true)]
        async fn filter_context_failure_skips_admission_until_the_next_tick() {
            let mut rig = memory_rig(true);
            let start = Instant::now();
            let pending = rig.commands.recv().await.unwrap();
            assert_eq!(Instant::now() - start, Duration::from_secs(3));
            assert_eq!(rig.contexts.recv().await, Some(1));
            assert_eq!(rig.contexts.recv().await, Some(2));
            assert!(rig.events.try_recv().is_err());
            stop(rig).await;
            drop(pending);
        }

        // Breaks concurrent observations or a Burst interval after a held read.
        #[tokio::test(start_paused = true)]
        async fn filter_held_read_is_serial_and_skips_missed_tick_debt() {
            let mut rig = memory_rig(false);
            let first = rig.commands.recv().await.unwrap();
            advance(Duration::from_secs(10)).await;
            assert!(rig.commands.try_recv().is_err());
            drop(first);
            drop(rig.commands.recv().await.unwrap());
            let completed = Instant::now();
            let next = rig.commands.recv().await.unwrap();
            assert_eq!(Instant::now() - completed, Duration::from_secs(2));
            stop(rig).await;
            drop(next);
        }

        #[tokio::test(start_paused = true)]
        async fn filter_offline_budget_abandons_the_old_request_without_replay() {
            let mut rig = memory_rig(false);
            let first = rig.commands.recv().await.unwrap();
            let start = Instant::now();
            advance(Duration::from_secs(40)).await;
            let next = rig.commands.recv().await.unwrap();
            assert_eq!(Instant::now() - start, Duration::from_secs(40));
            assert!(rig.events.try_recv().is_err());
            stop(rig).await;
            drop(next);
            tokio::time::resume();
            let (mut wire, mut peer) = Rig::start(1, &["session.panes"]).await;
            wire.commands.send(first).unwrap();
            wire.commands.send(ClientCommand::Discover).unwrap();
            assert!(matches!(peer.recv().await, MessageType::Discover));
            wire.stop().await;
        }

        #[tokio::test(start_paused = true)]
        async fn filter_cancellation_and_receiver_closure_drop_a_held_read() {
            for close_receiver in [false, true] {
                let mut rig = memory_rig(false);
                let pending = rig.commands.recv().await.unwrap();
                let now = Instant::now();
                if close_receiver {
                    drop(rig.events);
                } else {
                    rig.cancel.cancel();
                }
                rig.task.await.unwrap();
                assert_eq!(Instant::now(), now);
                assert!(rig.commands.recv().await.is_none());
                drop(pending);
            }
        }

        #[tokio::test]
        async fn filter_already_cancelled_never_admits_a_request() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let (events, _rx) = mpsc::unbounded_channel();
            let cancel = CancellationToken::new();
            cancel.cancel();
            guarded(spawn_filter_task(
                "work".into(),
                ControlHandle::new(tx),
                events,
                cancel,
                || Ok(caller()),
            ))
            .await
            .unwrap();
            assert!(rx.recv().await.is_none());
        }

        #[tokio::test]
        async fn filter_late_reply_cannot_complete_a_fresh_observation() {
            let (mut rig, mut peer) = Rig::start(1, &["session.panes"]).await;
            let first = query(rig.control());
            let old_id = panes(&mut peer).await;
            first.abort();
            assert!(guarded(first).await.unwrap_err().is_cancelled());
            let mut next = query(rig.control());
            let id = panes(&mut peer).await;
            assert_ne!(id, old_id);
            peer.send(DaemonMessage::response_ok(
                old_id,
                serde_json::json!({"pane_ids":[]}),
            ))
            .await;
            peer.send(DaemonMessage::session_removed("filter-late-barrier".into()))
                .await;
            loop {
                match guarded(rig.events.recv()).await.unwrap() {
                    Event::Control(_) => {}
                    Event::SessionRemoved(id) if id == "filter-late-barrier" => break,
                    other => panic!("late reply session flow: {other:?}"),
                }
            }
            std::future::poll_fn(|cx| {
                assert!(Pin::new(&mut next).poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            peer.send(DaemonMessage::response_ok(
                id,
                serde_json::json!({"pane_ids":["%42"]}),
            ))
            .await;
            assert_eq!(
                guarded(next).await.unwrap(),
                Some(HashSet::from(["%42".into()]))
            );
            rig.stop().await;
        }

        // A held membership read must not block navigation, Discover or quit.
        #[tokio::test]
        async fn filter_stall_keeps_navigation_discover_and_quit_live() {
            let (mut rig, mut peer) = Rig::start(1, &["session.panes"]).await;
            let task = spawn_filter_task(
                "work".into(),
                rig.control(),
                rig.event_tx.clone(),
                rig.cancel.clone(),
                || Ok(caller()),
            );
            let mut terminal = Terminal::with_options(
                CrosstermBackend::new(io::stdout()),
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
                },
            )
            .unwrap();
            let mut app = selected_app(Some("%42"));
            let (capture, _capture_rx) = tokio::sync::watch::channel(None);
            let (beads, _beads_rx) = tokio::sync::watch::channel(None);
            let control = rig.control();
            let input = rig.event_tx.clone();
            let key = |c| {
                Event::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Char(c),
                    KeyModifiers::NONE,
                ))
            };
            let script = async {
                panes(&mut peer).await;
                input.send(key('k')).unwrap();
                input.send(key('r')).unwrap();
                assert!(matches!(peer.recv().await, MessageType::Discover));
                input.send(key('q')).unwrap();
            };
            let (result, ()) = guarded(async {
                tokio::join!(
                    run_event_loop(
                        &mut terminal,
                        &mut app,
                        &mut rig.events,
                        &rig.commands,
                        &control,
                        &rig.cancel,
                        &capture,
                        &beads
                    ),
                    script
                )
            })
            .await;
            result.unwrap();
            guarded(task).await.unwrap();
            assert!(app.should_quit);
            assert!(app.selected_session().is_none());
            rig.stop().await;
        }

        #[tokio::test]
        #[ignore = "requires a sealed private daemon and disposable tmux servers"]
        async fn real_filter_task_matches_daemon_membership() {
            let run = std::path::PathBuf::from(std::env::var_os("ATM_FILTER_TEST_RUN").unwrap())
                .canonicalize()
                .unwrap();
            assert!(run.starts_with("/var/tmp"));
            assert!(run
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("cp-op3pq."));
            assert!(std::env::var_os("TMUX").is_none());
            assert!(std::env::var_os("TMUX_PANE").is_none());
            let caller = CallerContext::from_env().unwrap();
            let socket = std::path::PathBuf::from(std::env::var_os("ATM_SOCKET").unwrap());
            let foreign = std::env::var("ATM_FILTER_FOREIGN_SOCKET").unwrap();
            for path in [
                socket.as_path(),
                std::path::Path::new(&caller.caller_server),
                std::path::Path::new(&foreign),
            ] {
                assert!(path.canonicalize().unwrap().starts_with(&run));
            }
            let (commands, command_rx) = mpsc::unbounded_channel();
            let (events, mut event_rx) = mpsc::unbounded_channel();
            let cancel = CancellationToken::new();
            let daemon = DaemonClient::new(
                DaemonConfig {
                    socket_path: socket,
                    ..Default::default()
                },
                events.clone(),
                command_rx,
                cancel.clone(),
            );
            let daemon_task = tokio::spawn(async move { daemon.run().await });
            loop {
                if matches!(
                    guarded(event_rx.recv()).await,
                    Some(Event::SessionListReplace(_))
                ) {
                    break;
                }
            }
            let control = ControlHandle::new(commands);
            let task = spawn_filter_task(
                "work".into(),
                control.clone(),
                events,
                cancel.clone(),
                CallerContext::from_env,
            );
            loop {
                match guarded(event_rx.recv()).await.unwrap() {
                    Event::FilterUpdate(ids) => {
                        assert_eq!(ids, HashSet::from(["%0".into(), "%1".into()]));
                        break;
                    }
                    Event::Control(_) => {}
                    other => panic!("real membership event: {other:?}"),
                }
            }
            let mut app = App::with_tmux_session_filter("work".into());
            app.update_filter_panes(HashSet::from(["%0".into(), "%1".into()]));
            let wrong = CallerContext {
                caller_server: foreign,
                from_pane: None,
            };
            assert!(filter_session_panes(&control, "work", wrong)
                .await
                .is_none());
            assert_eq!(
                app.filter_pane_ids,
                HashSet::from(["%0".into(), "%1".into()])
            );
            app.update_filter_panes(
                filter_session_panes(&control, "missing", caller)
                    .await
                    .unwrap(),
            );
            assert!(app.filter_pane_ids.is_empty());
            cancel.cancel();
            guarded(task).await.unwrap();
            guarded(daemon_task).await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires a sealed private daemon and two disposable tmux servers"]
    async fn real_selected_pane_kill_and_wrong_server_refusal() {
        let run = std::path::PathBuf::from(std::env::var_os("ATM_KILL_TEST_RUN").unwrap())
            .canonicalize()
            .unwrap();
        assert!(run.starts_with("/var/tmp"));
        assert!(run
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("cp-op3kq."));
        assert!(std::env::var_os("TMUX").is_none());
        assert!(std::env::var_os("TMUX_PANE").is_none());
        let context = CallerContext::from_env().unwrap();
        let daemon_path = std::path::PathBuf::from(std::env::var_os("ATM_SOCKET").unwrap());
        let foreign_socket = std::env::var("ATM_KILL_TEST_FOREIGN_SOCKET").unwrap();
        let foreign_label = std::env::var("ATM_KILL_TEST_FOREIGN_LABEL").unwrap();
        let resolved_foreign = std::path::PathBuf::from(std::env::var_os("TMUX_TMPDIR").unwrap())
            .join(format!("tmux-{}", unsafe { libc::getuid() }))
            .join(&foreign_label);
        // Validate every configured socket AND its label before ANY tmux query or connect.
        for path in [
            std::path::Path::new(&context.caller_server),
            &daemon_path,
            std::path::Path::new(&foreign_socket),
        ] {
            assert!(path.canonicalize().unwrap().starts_with(&run));
        }
        assert_eq!(
            resolved_foreign.canonicalize().unwrap(),
            std::path::Path::new(&foreign_socket)
                .canonicalize()
                .unwrap()
        );
        let client = RealTmuxClient::new();
        let identity = client
            .raw_command("display-message", &["-p", "#{socket_path}\t#{pid}"])
            .await
            .unwrap();
        let (socket, pid) = identity.trim().split_once('\t').unwrap();
        assert_eq!(context.caller_server, socket);
        assert!(pid.parse::<u32>().unwrap() > 1);
        let foreign = RealTmuxClient::with_socket(foreign_label);
        let identity = foreign
            .raw_command("display-message", &["-p", "#{socket_path}\t#{pid}"])
            .await
            .unwrap();
        let (socket, pid) = identity.trim().split_once('\t').unwrap();
        assert_eq!(foreign_socket, socket);
        assert!(pid.parse::<u32>().unwrap() > 1);
        let pane = std::env::var("ATM_KILL_TEST_PANE").unwrap();
        let keeper = std::env::var("ATM_KILL_TEST_KEEPER").unwrap();
        assert_ne!(pane, keeper);
        let before = HashSet::from([pane.clone(), keeper.clone()]);
        let ids = |panes: Vec<atm_tmux::PaneInfo>| {
            panes.into_iter().map(|p| p.pane_id).collect::<HashSet<_>>()
        };
        assert_eq!(ids(client.list_panes().await.unwrap()), before);
        assert_eq!(ids(foreign.list_panes().await.unwrap()), before);
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, mut event_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let daemon = DaemonClient::new(
            DaemonConfig {
                socket_path: daemon_path,
                ..Default::default()
            },
            events,
            command_rx,
            cancel.clone(),
        );
        let daemon_task = tokio::spawn(async move { daemon.run().await });
        assert!(matches!(
            guarded(event_rx.recv()).await,
            Some(Event::SessionListReplace(_))
        ));
        let control = ControlHandle::new(commands);
        let (logs, _guard) = Logs::capture();
        let wrong = CallerContext {
            caller_server: foreign_socket,
            from_pane: Some(keeper.clone()),
        };
        guarded(
            spawn_kill_selected_pane(
                &selected_app(Some(&pane)),
                control.clone(),
                wrong,
                cancel.clone(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        assert!(
            logs.text().contains("atmd controls tmux server"),
            "{}",
            logs.text()
        );
        assert_eq!(ids(client.list_panes().await.unwrap()), before);
        assert_eq!(ids(foreign.list_panes().await.unwrap()), before);
        guarded(
            spawn_kill_selected_pane(&selected_app(Some(&pane)), control, context, cancel.clone())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            ids(client.list_panes().await.unwrap()),
            HashSet::from([keeper])
        );
        assert_eq!(ids(foreign.list_panes().await.unwrap()), before);
        cancel.cancel();
        guarded(daemon_task).await.unwrap();
    }
}

#[cfg(test)]
mod interrupt_action_tests {
    use super::*;
    use atm_core::{SessionView, TreeRowKind};
    use atm_tui::client::{DaemonClient, DaemonConfig};

    fn caller() -> CallerContext {
        CallerContext {
            from_pane: Some("%7".into()),
            caller_server: "/private/caller-server".into(),
        }
    }

    async fn finished(action: tokio::task::JoinHandle<()>) {
        tokio::time::timeout(Duration::from_secs(60), action)
            .await
            .expect("hung interrupt action")
            .expect("interrupt action panicked");
    }

    fn selected_app(pane: Option<&str>) -> App {
        let mut app = App::new();
        app.view = atm_tui::app::AppView::Sessions;
        app.update_sessions(vec![SessionView {
            id: "selected".into(),
            project_root: Some("/project".into()),
            tmux_pane: pane.map(str::to_owned),
            ..SessionView::default()
        }]);
        app.selected_index = app
            .tree_rows
            .iter()
            .position(|row| matches!(row.kind, TreeRowKind::Agent { .. }))
            .unwrap();
        app
    }

    // Empty, group and invalid selections must not create a worker or request.
    #[tokio::test]
    async fn no_selection_does_not_start_an_interrupt() {
        let mut group = selected_app(Some("%42"));
        group.select_go_to(0);
        assert!(group.selected_session().is_none());
        let mut out_of_bounds = selected_app(Some("%42"));
        out_of_bounds.selected_index = out_of_bounds.tree_rows.len();
        for app in [App::new(), group, out_of_bounds] {
            let (tx, mut rx) = mpsc::unbounded_channel();
            assert!(spawn_interrupt_selected_pane(
                &app,
                ControlHandle::new(tx),
                caller(),
                CancellationToken::new()
            )
            .is_none());
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn selected_session_without_a_pane_does_not_start_an_interrupt() {
        let app = selected_app(None);
        assert!(app.selected_session().is_some());
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(spawn_interrupt_selected_pane(
            &app,
            ControlHandle::new(tx),
            caller(),
            CancellationToken::new()
        )
        .is_none());
        assert!(rx.try_recv().is_err());
    }

    // Exact target/error assertions now live in interrupt_request_tests above;
    // the daemon's pane_request_tests retain the exact C-c effect assertion.

    #[tokio::test]
    #[ignore = "requires a sealed private daemon, tmux server and foreground SIGINT stand-in"]
    async fn real_selected_pane_interrupt_matches_direct_baseline() {
        let run = std::path::PathBuf::from(std::env::var_os("ATM_INTERRUPT_TEST_RUN").unwrap())
            .canonicalize()
            .unwrap();
        assert!(run.starts_with("/var/tmp"));
        assert!(run
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("cp-op3iq."));
        assert!(std::env::var_os("TMUX").is_none());
        assert!(std::env::var_os("TMUX_PANE").is_none());
        let mut caller = CallerContext::from_env().unwrap();
        // Validate the resolved caller socket and the seal before any tmux I/O.
        assert!(std::path::Path::new(&caller.caller_server)
            .canonicalize()
            .unwrap()
            .starts_with(&run));
        for key in ["ATM_SOCKET", "TMUX_TMPDIR", "ATM_STATE_DIR", "HOME"] {
            let path = std::path::PathBuf::from(std::env::var_os(key).unwrap());
            assert!(path.starts_with(&run));
        }
        let client = RealTmuxClient::new();
        let identity = client
            .raw_command("display-message", &["-p", "#{socket_path}\t#{pid}"])
            .await
            .unwrap();
        let (socket, pid) = identity.trim().split_once('\t').unwrap();
        assert_eq!(caller.caller_server, socket);
        assert!(pid.parse::<u32>().unwrap() > 1);
        let pane = std::env::var("ATM_INTERRUPT_TEST_PANE").unwrap();
        let keeper = std::env::var("ATM_INTERRUPT_TEST_KEEPER").unwrap();
        assert_ne!(pane, keeper);
        assert_eq!(
            client
                .list_panes()
                .await
                .unwrap()
                .into_iter()
                .map(|p| p.pane_id)
                .collect::<HashSet<_>>(),
            HashSet::from([pane.clone(), keeper])
        );
        let socket_path = std::path::PathBuf::from(std::env::var_os("ATM_SOCKET").unwrap());
        assert!(socket_path.canonicalize().unwrap().starts_with(&run));
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, mut event_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let daemon = DaemonClient::new(
            DaemonConfig {
                socket_path,
                ..Default::default()
            },
            events,
            command_rx,
            cancel.clone(),
        );
        let daemon_task = tokio::spawn(async move { daemon.run().await });
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(60), event_rx.recv())
                .await
                .unwrap(),
            Some(Event::SessionListReplace(_))
        ));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_test_writer()
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        caller.from_pane = Some(std::env::var("ATM_INTERRUPT_TEST_KEEPER").unwrap());
        finished(
            spawn_interrupt_selected_pane(
                &selected_app(Some(&pane)),
                ControlHandle::new(commands),
                caller,
                cancel.clone(),
            )
            .unwrap(),
        )
        .await;
        cancel.cancel();
        finished(daemon_task).await;
        // The rig checks the refusal's ordered no-input marker, then SIGINT on
        // release; the real daemon verifies attribution for both audit records.
    }
}

#[cfg(test)]
mod role_task_tests {
    use super::*;
    use tokio::time::{advance, Instant};

    struct Rig {
        commands: mpsc::UnboundedReceiver<ClientCommand>,
        events: mpsc::UnboundedReceiver<Event>,
        cancel: CancellationToken,
        handle: tokio::task::JoinHandle<()>,
    }
    fn rig() -> Rig {
        let (commands_tx, commands) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let handle = spawn_role_task(ControlHandle::new(commands_tx), event_tx, cancel.clone());
        Rig {
            commands,
            events,
            cancel,
            handle,
        }
    }
    async fn command(rig: &mut Rig) -> ClientCommand {
        tokio::time::timeout(Duration::from_secs(60), rig.commands.recv())
            .await
            .expect("role poll did not submit")
            .unwrap()
    }
    async fn stop(rig: Rig) {
        rig.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(60), rig.handle)
            .await
            .expect("role poll did not stop")
            .unwrap();
    }

    // Dropping an opaque command returns Closed. Only in-memory I/O runs with
    // the paused clock; the production poll, budget and interval remain real.
    #[tokio::test(start_paused = true)]
    async fn role_poll_is_immediate_then_three_seconds_without_failure_retry() {
        let mut rig = rig();
        let start = Instant::now();
        drop(command(&mut rig).await);
        assert_eq!(Instant::now(), start);
        drop(command(&mut rig).await);
        assert_eq!(Instant::now() - start, Duration::from_secs(3));
        drop(command(&mut rig).await);
        assert_eq!(Instant::now() - start, Duration::from_secs(6));
        assert!(rig.events.try_recv().is_err());
        stop(rig).await;
    }

    #[tokio::test(start_paused = true)]
    async fn role_poll_has_one_slot_and_skips_missed_ticks() {
        let mut rig = rig();
        let start = Instant::now();
        let first = command(&mut rig).await;
        advance(Duration::from_secs(10)).await;
        assert!(rig.commands.try_recv().is_err());
        drop(first);
        drop(command(&mut rig).await);
        assert_eq!(Instant::now() - start, Duration::from_secs(10));
        drop(command(&mut rig).await);
        assert_eq!(Instant::now() - start, Duration::from_secs(12));
        stop(rig).await;
    }

    #[tokio::test(start_paused = true)]
    async fn role_poll_cancels_or_closes_while_waiting_for_a_reply() {
        for closed in [false, true] {
            let mut rig = rig();
            let pending = command(&mut rig).await;
            let now = Instant::now();
            if closed {
                drop(rig.events);
            } else {
                rig.cancel.cancel();
            }
            rig.handle.await.unwrap();
            assert_eq!(Instant::now(), now);
            assert!(rig.commands.recv().await.is_none());
            drop(pending);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn role_poll_budget_includes_offline_time_without_retry() {
        let mut rig = rig();
        let first = command(&mut rig).await;
        let start = Instant::now();
        advance(Duration::from_secs(40)).await;
        let next = command(&mut rig).await;
        assert_eq!(Instant::now() - start, Duration::from_secs(40));
        assert!(rig.events.try_recv().is_err());
        stop(rig).await;
        drop((first, next));
    }

    #[tokio::test(start_paused = true)]
    async fn an_already_canceled_role_poll_never_queues() {
        let rig = rig();
        rig.cancel.cancel();
        rig.handle.await.unwrap();
        assert!(rig.commands.is_empty());
        assert!(rig.events.is_empty());
    }
}

#[cfg(test)]
mod capture_task_tests {
    use super::*;
    use tokio::sync::watch;
    use tokio::time::{advance, Instant};

    struct Rig {
        commands: mpsc::UnboundedReceiver<ClientCommand>,
        events: mpsc::UnboundedReceiver<Event>,
        target: watch::Sender<Option<String>>,
        cancel: CancellationToken,
        handle: tokio::task::JoinHandle<()>,
    }

    // An offline in-memory command queue: dropping a received command returns
    // Closed. No real sockets are combined with Tokio's paused clock.
    fn rig(start: Option<&str>) -> Rig {
        let (commands_tx, commands) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let (target, target_rx) = watch::channel(start.map(str::to_owned));
        let cancel = CancellationToken::new();
        let handle = spawn_capture_task(
            ControlHandle::new(commands_tx),
            event_tx,
            cancel.clone(),
            target_rx,
        );
        Rig {
            commands,
            events,
            target,
            cancel,
            handle,
        }
    }

    async fn command(rig: &mut Rig) -> ClientCommand {
        tokio::time::timeout(Duration::from_secs(60), rig.commands.recv())
            .await
            .expect("capture did not submit")
            .expect("commands closed")
    }

    async fn stop(rig: Rig) {
        rig.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(60), rig.handle)
            .await
            .expect("capture did not stop")
            .unwrap();
    }

    // Catches eager retry on error and a cadence that drifts or bursts.
    #[tokio::test(start_paused = true)]
    async fn the_target_keeps_refreshing_on_the_tick_without_error_retry() {
        let mut rig = rig(Some("%1"));
        let start = Instant::now();
        drop(command(&mut rig).await);
        assert_eq!(Instant::now(), start);
        drop(command(&mut rig).await);
        assert_eq!(Instant::now() - start, Duration::from_secs(1));
        drop(command(&mut rig).await);
        assert_eq!(Instant::now() - start, Duration::from_secs(2));
        assert!(rig.events.try_recv().is_err());
        stop(rig).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_target_is_captured_without_waiting_for_the_tick() {
        let mut rig = rig(Some("%1"));
        let first = command(&mut rig).await;
        let now = Instant::now();
        rig.target.send_replace(Some("%2".into()));
        drop(first);
        let second = command(&mut rig).await;
        assert_eq!(Instant::now(), now);
        stop(rig).await;
        drop(second);
    }

    #[tokio::test(start_paused = true)]
    async fn empty_targets_submit_nothing_and_first_selection_is_immediate() {
        for target in [None, Some("")] {
            let mut rig = rig(target);
            advance(Duration::from_secs(3)).await;
            assert!(rig.commands.try_recv().is_err());
            let now = Instant::now();
            rig.target.send_replace(Some("%42".into()));
            let pending = command(&mut rig).await;
            assert_eq!(Instant::now(), now);
            stop(rig).await;
            drop(pending);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn clearing_the_target_captures_nothing() {
        let mut rig = rig(Some("%1"));
        let pending = command(&mut rig).await;
        rig.target.send_replace(None);
        drop(pending);
        advance(Duration::from_secs(3)).await;
        assert!(rig.commands.try_recv().is_err());
        assert!(rig.events.try_recv().is_err());
        stop(rig).await;
    }

    // A stalled request is the only owned slot even after many missed ticks.
    // The first completion consumes one overdue tick, then Skip prevents a burst.
    #[tokio::test(start_paused = true)]
    async fn held_capture_is_serial_and_missed_ticks_do_not_burst() {
        let mut rig = rig(Some("%1"));
        let first = command(&mut rig).await;
        advance(Duration::from_secs(10)).await;
        assert!(rig.commands.try_recv().is_err());
        drop(first);
        drop(command(&mut rig).await);
        let completed = Instant::now();
        drop(command(&mut rig).await);
        assert_eq!(Instant::now() - completed, Duration::from_secs(1));
        stop(rig).await;
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_stops_a_held_capture_without_waiting_for_its_budget() {
        let mut rig = rig(Some("%42"));
        let pending = command(&mut rig).await;
        let now = Instant::now();
        rig.cancel.cancel();
        rig.handle.await.unwrap();
        assert_eq!(Instant::now(), now);
        assert!(rig.events.recv().await.is_none());
        assert!(rig.commands.recv().await.is_none());
        drop(pending);
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_after_cancel_ends_the_task_without_capturing() {
        let mut rig = rig(None);
        rig.cancel.cancel();
        rig.target.send_replace(Some("%1".into()));
        rig.handle.await.unwrap();
        assert!(rig.events.recv().await.is_none());
        assert!(rig.commands.recv().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_capture_releases_the_slot_without_an_event() {
        let mut rig = rig(Some("%42"));
        let first = command(&mut rig).await;
        rig.target.send_replace(None);
        let now = Instant::now();
        advance(Duration::from_secs(40)).await;
        assert!(rig.events.try_recv().is_err());
        rig.target.send_replace(Some("%2".into()));
        let second = command(&mut rig).await;
        assert_eq!(Instant::now() - now, Duration::from_secs(40));
        stop(rig).await;
        drop((first, second));
    }

    #[tokio::test]
    async fn a_dropped_sender_ends_the_task() {
        let rig = rig(None);
        drop(rig.target);
        tokio::time::timeout(Duration::from_secs(60), rig.handle)
            .await
            .expect("capture spun on the closed target")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_target_drains_in_flight_and_latest_captures_serially() {
        let mut rig = rig(Some("%1"));
        let first = command(&mut rig).await;
        rig.target.send_replace(Some("%2".into()));
        drop(rig.target);
        assert!(rig.commands.try_recv().is_err());
        drop(first);
        let second = rig.commands.recv().await.unwrap();
        assert!(rig.commands.try_recv().is_err());
        drop(second);
        rig.handle.await.unwrap();
        assert!(rig.commands.recv().await.is_none());
        assert!(rig.events.recv().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_event_receiver_cancels_the_in_flight_capture() {
        let mut rig = rig(Some("%1"));
        let pending = command(&mut rig).await;
        drop(rig.events);
        let now = Instant::now();
        rig.handle.await.unwrap();
        assert_eq!(Instant::now(), now);
        assert!(rig.commands.recv().await.is_none());
        drop(pending);
    }

    #[tokio::test]
    #[ignore = "requires a sealed private atmd/tmux and terminal-response-gated painter"]
    async fn real_visible_preview_matches_direct_baseline() {
        use atm_core::{SessionView, TreeRowKind};
        use atm_tui::client::DaemonClient;
        let run = std::path::PathBuf::from(std::env::var_os("ATM_CAPTURE_TEST_RUN").unwrap())
            .canonicalize()
            .unwrap();
        assert!(run
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("cp-op3cq."));
        assert!(std::env::var_os("TMUX").is_none());
        assert!(std::env::var_os("TMUX_PANE").is_none());
        assert_eq!(std::env::var("ATM_TMUX_SOCKET").unwrap(), "op3cq");
        for key in ["ATM_SOCKET", "TMUX_TMPDIR", "ATM_STATE_DIR", "HOME"] {
            assert!(std::path::PathBuf::from(std::env::var_os(key).unwrap()).starts_with(&run));
        }
        let client = RealTmuxClient::new();
        let identity = client
            .raw_command("display-message", &["-p", "#{socket_path}\t#{pid}"])
            .await
            .unwrap();
        let (socket, pid) = identity.trim().split_once('\t').unwrap();
        assert!(std::path::Path::new(socket)
            .canonicalize()
            .unwrap()
            .starts_with(&run));
        assert!(pid.parse::<u32>().unwrap() > 1);
        let pane = std::env::var("ATM_CAPTURE_TEST_PANE").unwrap();
        let keeper = std::env::var("ATM_CAPTURE_TEST_KEEPER").unwrap();
        let keeper_lines = client.capture_pane(&keeper).await.unwrap();
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let (commands, command_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let daemon = DaemonClient::with_defaults(event_tx.clone(), command_rx, cancel.clone());
        let connection = tokio::spawn(async move { daemon.run().await });
        let (target, target_rx) = watch::channel(None);
        let handle = spawn_capture_task(
            ControlHandle::new(commands),
            event_tx,
            cancel.clone(),
            target_rx,
        );
        let mut app = App::with_tmux_session_filter("probe".into());
        app.update_filter_panes([pane.clone(), keeper.clone()].into_iter().collect());
        app.update_sessions(vec![
            SessionView {
                id: "painted".into(),
                tmux_pane: Some(pane.clone()),
                ..SessionView::default()
            },
            SessionView {
                id: "keeper".into(),
                tmux_pane: Some(keeper.clone()),
                ..SessionView::default()
            },
        ]);
        for (selected, expected) in [
            (
                "painted",
                vec![
                    "preview alpha".into(),
                    "preview beta café λ".into(),
                    "".into(),
                    "preview omega".into(),
                ],
            ),
            ("keeper", keeper_lines),
            (
                "painted",
                vec![
                    "preview alpha".into(),
                    "preview beta café λ".into(),
                    "".into(),
                    "preview omega".into(),
                ],
            ),
        ] {
            if selected == "painted" && app.capture_pane_id.as_deref() == Some(&keeper) {
                app.update_filter_panes([pane.clone()].into_iter().collect());
                assert!(app.tree_rows.iter().all(|row| !matches!(&row.kind, TreeRowKind::Agent { session } if session.id.as_str() == "keeper")));
            }
            app.selected_index = app.tree_rows.iter().position(|row| matches!(&row.kind, TreeRowKind::Agent { session } if session.id.as_str() == selected)).unwrap();
            sync_capture_target(&mut app, &target);
            let event = tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    if let Some(Event::CaptureUpdate { pane_id, lines }) = events.recv().await {
                        if Some(&pane_id) == app.capture_pane_id.as_ref() {
                            break (pane_id, lines);
                        }
                    }
                }
            })
            .await
            .expect("daemon preview stalled");
            assert_eq!(event.1, expected);
            app.update_capture(&event.0, event.1);
            println!(
                "daemon preview selected={selected} pane={} lines={:?}",
                event.0, app.captured_output
            );
        }
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(60), handle)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(60), connection)
            .await
            .unwrap()
            .unwrap();
    }
}
