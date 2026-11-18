//! CLI and RAII orchestration for the independent chat terminal.
use anyhow::{anyhow, Result};
use atm_chat::ChatConfig;
use atm_comms::Address;
use atm_tmux::RealTmuxClient;
use atm_tui::{client::DaemonClient, daemon::ControlHandle};
use chrono::Utc;
use clap::Args;
use crossterm::{
    cursor::Show,
    execute,
    terminal::{disable_raw_mode, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};
use std::{sync::Mutex, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::chat::{self, ChatRunId};

#[derive(Debug, Args)]
pub(super) struct ChatArgs {
    /// Initial full address (project/name, owner or system); never a bare-name lookup
    #[arg(long)]
    pub thread: Option<Address>,
}

struct TerminalOwner(Option<Terminal<CrosstermBackend<Stdout>>>);
impl TerminalOwner {
    fn open() -> Result<Self> {
        // The empty guard covers setup_terminal failing after enabling raw mode
        // or entering alternate screen, before it can return a Terminal.
        let mut owner = Self(None);
        owner.0 = Some(super::tui::setup_terminal()?);
        Ok(owner)
    }
}
impl Drop for TerminalOwner {
    fn drop(&mut self) {
        let cleaned = self
            .0
            .as_mut()
            .is_some_and(|terminal| super::tui::cleanup_terminal(terminal).is_ok());
        if !cleaned {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
        }
    }
}

struct Background {
    cancel: CancellationToken,
    handles: Vec<JoinHandle<()>>,
}
impl Background {
    async fn finish(&mut self) -> Result<()> {
        self.cancel.cancel();
        let mut failed = false;
        for handle in &mut self.handles {
            match tokio::time::timeout(Duration::from_secs(30), &mut *handle).await {
                Ok(result) => failed |= result.is_err(),
                Err(_) => {
                    failed = true;
                    handle.abort();
                    let _ = tokio::time::timeout(Duration::from_secs(30), handle).await;
                }
            }
        }
        self.handles.clear();
        if failed {
            Err(anyhow!("chat background shutdown failed"))
        } else {
            Ok(())
        }
    }
}
impl Drop for Background {
    fn drop(&mut self) {
        self.cancel.cancel();
        for handle in &self.handles {
            handle.abort();
        }
    }
}

async fn open_log(
    open: impl FnOnce() -> Option<std::fs::File> + Send + 'static,
    budget: Duration,
) -> Result<Option<std::fs::File>> {
    let (send, receive) = tokio::sync::oneshot::channel();
    // An existing tui.log can be a FIFO or live on a stuck filesystem. A
    // spawn_blocking task would keep runtime shutdown waiting forever. Exactly
    // one startup thread is allowed; a timeout fails BEFORE terminal setup, so
    // neither late diagnostics nor a stuck worker can corrupt a running screen.
    let worker = std::thread::Builder::new()
        .name("atm-chat-log".into())
        .spawn(move || {
            // Even an already-open pipe can later fill and block log writes.
            let file =
                open().filter(|file| file.metadata().is_ok_and(|metadata| metadata.is_file()));
            let _ = send.send(file);
        })?;
    let result = tokio::time::timeout(budget, receive)
        .await
        .map_err(|_| anyhow!("chat log initialization timed out"))?;
    // The notification follows all I/O; only the thread's return remains.
    let _ = worker.join();
    Ok(result.unwrap_or(None))
}

pub(super) async fn cmd_chat(args: ChatArgs) -> Result<()> {
    let run = ChatRunId::new(Utc::now(), std::process::id());
    // Logging is initialized before raw mode; errors cannot corrupt the screen.
    let log = open_log(super::create_log_file, Duration::from_secs(30)).await?;
    if let Some(file) = log {
        if tracing_subscriber::fmt()
            .with_ansi(false)
            .log_internal_errors(false)
            .with_env_filter("atm::chat=info")
            .with_writer(Mutex::new(file))
            .try_init()
            .is_err()
        {
            eprintln!("chat measurements unavailable: logging already initialized");
        }
    } else {
        eprintln!("chat measurements unavailable: log file could not be opened");
    }
    let mut terminal = TerminalOwner::open()?;
    let cancel = CancellationToken::new();
    let (event_tx, events) = mpsc::unbounded_channel();
    let (commands, command_rx) = mpsc::unbounded_channel();
    let control = ControlHandle::new(commands);
    let client = DaemonClient::with_defaults(event_tx.clone(), command_rx, cancel.clone());
    let mut background = Background {
        cancel: cancel.clone(),
        handles: vec![
            tokio::spawn(async move { client.run().await }),
            super::tui::spawn_keyboard_task(event_tx, cancel.clone()),
        ],
    };
    let tmux = RealTmuxClient::new();
    let config = ChatConfig {
        initial_thread: args.thread,
        caller_pane: std::env::var("TMUX_PANE")
            .ok()
            .filter(|pane| !pane.is_empty()),
    };
    let result = match terminal.0.as_mut() {
        Some(terminal) => {
            let frame_run = run.clone();
            let mut traced_frame = None;
            chat::drive(
                terminal,
                config,
                control,
                events,
                &tmux,
                cancel,
                run.clone(),
                move |report| {
                    chat::trace_frame(&frame_run, report, &mut traced_frame);
                },
            )
            .await
        }
        None => Err(anyhow!("chat terminal unavailable")),
    };
    let shutdown = background.finish().await;
    drop(terminal);
    if shutdown.is_err() {
        tracing::warn!(target:"atm::chat",run_id=%run,"chat.shutdown_failed");
    }
    result?;
    shutdown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn log_sink_with_a_reader_must_still_be_a_regular_file() {
        use std::os::unix::fs::OpenOptionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("log.fifo");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .unwrap();
        let log = tokio::time::timeout(
            Duration::from_secs(60),
            open_log(move || Some(file), Duration::from_secs(30)),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            log.is_none(),
            "an open FIFO can fill and block synchronous logging after startup"
        );
    }

    #[tokio::test]
    async fn log_open_is_off_runtime_and_finished_before_return() {
        let path = tempfile::NamedTempFile::new().unwrap();
        let path = path.path().to_owned();
        let caller = std::thread::current().id();
        let observed = std::sync::Arc::new(Mutex::new(None));
        let worker = observed.clone();
        let file = tokio::time::timeout(
            Duration::from_secs(60),
            open_log(
                move || {
                    *worker.lock().unwrap() = Some(std::thread::current().id());
                    std::fs::File::open(path).ok()
                },
                Duration::from_secs(30),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(file.is_some());
        assert!(observed.lock().unwrap().is_some_and(|id| id != caller));
    }
}
