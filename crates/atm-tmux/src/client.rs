//! Real tmux client implementation.
//!
//! Shells out to the `tmux` binary via `tokio::process::Command`.

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, trace};

use crate::{PaneDirection, PaneInfo, TmuxClient, TmuxError};

/// Deadline for one tmux command. tmux normally answers in milliseconds, but
/// a stopped or wedged server never answers and the client waits forever.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Deadline for `new-session`, the one command that can start the server.
/// tmux holds the starting client until the user's tmux.conf has loaded,
/// including synchronous `run-shell` lines (e.g. a first-run plugin install).
const NEW_SESSION_TIMEOUT: Duration = Duration::from_secs(30);

/// The deadline for one run of `subcommand`.
fn command_timeout(subcommand: &str) -> Duration {
    if subcommand == "new-session" {
        NEW_SESSION_TIMEOUT
    } else {
        COMMAND_TIMEOUT
    }
}

/// A paste-buffer name no other live `atm` process or call is using.
/// Buffers belong to the whole tmux server, not to one client, and a pid
/// can repeat across PID namespaces that share one server (a container
/// using the host's socket), so the name carries the time too.
fn unique_buffer_name() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "atm-send-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Real tmux client that invokes the `tmux` CLI.
///
/// Each method maps to a single tmux subcommand. The client is stateless —
/// all state lives in the tmux server.
#[derive(Debug, Clone, Default)]
pub struct RealTmuxClient {
    /// Optional socket name for connecting to a specific tmux server.
    /// When `None`, uses the default server. Set via [`RealTmuxClient::with_socket`]
    /// for integration tests that run an isolated tmux server.
    socket_name: Option<String>,
}

/// Environment variable read by [`RealTmuxClient::new`] to pick a non-default
/// tmux socket label. Mirrors `ATM_SOCKET` for the daemon side: lets test
/// harnesses redirect every tmux call an `atm` process makes to a private
/// `tmux -L <label>` server without modifying call sites.
pub const TMUX_SOCKET_ENV: &str = "ATM_TMUX_SOCKET";

impl RealTmuxClient {
    /// Creates a new client.
    ///
    /// If the [`TMUX_SOCKET_ENV`] environment variable is set, the client
    /// targets that tmux socket label (`tmux -L <label>`). Otherwise it
    /// uses the default tmux server.
    pub fn new() -> Self {
        match std::env::var(TMUX_SOCKET_ENV) {
            Ok(label) if !label.is_empty() => Self::with_socket(label),
            _ => Self::default(),
        }
    }

    /// Creates a client targeting a specific tmux server socket.
    ///
    /// Useful for integration tests: spin up `tmux -L <name>` and interact
    /// with it in isolation from the user's real tmux sessions.
    pub fn with_socket(name: impl Into<String>) -> Self {
        Self {
            socket_name: Some(name.into()),
        }
    }

    /// Builds a `Command` with the base `tmux` invocation.
    /// Adds `-L <socket>` if a custom socket name is configured.
    fn tmux_cmd(&self) -> Command {
        let mut cmd = Command::new("tmux");
        if let Some(ref socket) = self.socket_name {
            cmd.arg("-L").arg(socket);
        }
        cmd
    }

    /// Runs a tmux command, returning stdout on success or `TmuxError` on failure.
    ///
    /// Gives up after [`command_timeout`] with [`TmuxError::Timeout`]; the
    /// `tmux` process is killed rather than left blocked on the server.
    async fn run(&self, subcommand: &str, args: &[&str]) -> Result<String, TmuxError> {
        self.run_with_stdin(subcommand, args, None).await
    }

    /// Like [`Self::run`], writing `stdin` (when given) to the command's
    /// standard input. The deadline covers the write too.
    async fn run_with_stdin(
        &self,
        subcommand: &str,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<String, TmuxError> {
        trace!(subcommand, ?args, "running tmux command");

        let timeout = command_timeout(subcommand);
        let mut cmd = self.tmux_cmd();
        cmd.arg(subcommand).args(args).kill_on_drop(true);
        let output = async {
            let Some(input) = stdin else {
                return cmd.output().await;
            };
            let mut child = cmd
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            if let Some(mut pipe) = child.stdin.take() {
                // A failed write means tmux stopped reading (no server, say);
                // its exit status and stderr say why. Dropping the pipe
                // closes it, so tmux sees end of input.
                let _ = pipe.write_all(input.as_bytes()).await;
            }
            child.wait_with_output().await
        };
        let output = tokio::time::timeout(timeout, output)
            .await
            .map_err(|_| TmuxError::Timeout {
                command: subcommand.to_string(),
                timeout,
            })?
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    TmuxError::NotFound
                } else {
                    TmuxError::Io(e)
                }
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            debug!(subcommand, %stderr, "tmux command failed");
            return Err(TmuxError::CommandFailed {
                command: subcommand.to_string(),
                stderr,
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Runs a tmux command that produces no meaningful output.
    async fn run_silent(&self, subcommand: &str, args: &[&str]) -> Result<(), TmuxError> {
        self.run(subcommand, args).await.map(|_| ())
    }
}

#[async_trait]
impl TmuxClient for RealTmuxClient {
    async fn raw_command(&self, subcommand: &str, args: &[&str]) -> Result<String, TmuxError> {
        self.run(subcommand, args).await
    }

    async fn split_window(
        &self,
        target: &str,
        size: &str,
        direction: PaneDirection,
        command: Option<&str>,
    ) -> Result<String, TmuxError> {
        let (axis_flag, before) = match direction {
            PaneDirection::Left => ("-h", true),
            PaneDirection::Right => ("-h", false),
            PaneDirection::Above => ("-v", true),
            PaneDirection::Below => ("-v", false),
        };
        let mut args = vec![
            "-t",
            target,
            axis_flag,
            "-l",
            size,
            "-P", // print info about the new pane
            "-F",
            "#{pane_id}",
        ];
        if before {
            args.push("-b");
        }
        if let Some(cmd) = command {
            args.push(cmd);
        }

        let output = self.run("split-window", &args).await?;
        let pane_id = output.trim().to_string();
        if pane_id.is_empty() {
            return Err(TmuxError::ParseError(
                "split-window returned empty pane ID".to_string(),
            ));
        }
        debug!(%pane_id, ?direction, "split-window created new pane");
        Ok(pane_id)
    }

    async fn new_window(&self, session: &str, command: Option<&str>) -> Result<String, TmuxError> {
        let mut args = vec!["-t", session, "-P", "-F", "#{pane_id}"];
        if let Some(cmd) = command {
            args.push(cmd);
        }

        let output = self.run("new-window", &args).await?;
        let pane_id = output.trim().to_string();
        if pane_id.is_empty() {
            return Err(TmuxError::ParseError(
                "new-window returned empty pane ID".to_string(),
            ));
        }
        debug!(%pane_id, "new-window created new pane");
        Ok(pane_id)
    }

    async fn kill_pane(&self, pane: &str) -> Result<(), TmuxError> {
        self.run_silent("kill-pane", &["-t", pane]).await
    }

    async fn resize_pane(
        &self,
        pane: &str,
        width: Option<u16>,
        height: Option<u16>,
    ) -> Result<(), TmuxError> {
        let mut args = vec!["-t".to_string(), pane.to_string()];
        if let Some(w) = width {
            args.push("-x".to_string());
            args.push(w.to_string());
        }
        if let Some(h) = height {
            args.push("-y".to_string());
            args.push(h.to_string());
        }

        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.run_silent("resize-pane", &arg_refs).await
    }

    async fn send_keys(&self, pane: &str, keys: &str) -> Result<(), TmuxError> {
        self.run_silent("send-keys", &["-t", pane, keys]).await
    }

    async fn paste_text(&self, pane: &str, text: &str) -> Result<(), TmuxError> {
        if text.is_empty() {
            // `load-buffer -` makes no buffer from empty input (tmux 3.8),
            // so the paste would fail on a missing buffer.
            return Ok(());
        }
        let buffer = unique_buffer_name();
        // `-p` brackets the paste when the program asked for it; `-d`
        // deletes the buffer, but only once the paste has happened.
        let pasted = match self
            .run_with_stdin("load-buffer", &["-b", &buffer, "-"], Some(text))
            .await
        {
            Ok(_) => {
                self.run_silent("paste-buffer", &["-p", "-d", "-b", &buffer, "-t", pane])
                    .await
            }
            Err(error) => Err(error),
        };
        // A load that timed out after sending its text can still have made
        // the buffer, so clean up after either failure.
        if pasted.is_err() {
            let _ = self.run_silent("delete-buffer", &["-b", &buffer]).await;
        }
        pasted
    }

    async fn list_panes(&self) -> Result<Vec<PaneInfo>, TmuxError> {
        let format = "#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_pid}\t#{pane_width}\t#{pane_height}\t#{pane_active}";
        let output = self.run("list-panes", &["-a", "-F", format]).await?;

        let mut panes = Vec::new();
        for line in output.lines() {
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            let Some(pane_id) = fields.first() else {
                continue;
            };
            let Some(session_name) = fields.get(1) else {
                continue;
            };
            let pane = PaneInfo {
                pane_id: pane_id.to_string(),
                session_name: session_name.to_string(),
                window_index: fields.get(2).and_then(|s| s.parse().ok()).unwrap_or(0),
                pane_pid: fields.get(3).and_then(|s| s.parse().ok()).unwrap_or(0),
                width: fields.get(4).and_then(|s| s.parse().ok()).unwrap_or(0),
                height: fields.get(5).and_then(|s| s.parse().ok()).unwrap_or(0),
                is_active: fields.get(6).is_some_and(|s| *s == "1"),
            };
            panes.push(pane);
        }
        Ok(panes)
    }

    async fn display_popup(
        &self,
        width: &str,
        height: &str,
        command: &str,
    ) -> Result<(), TmuxError> {
        self.run_silent("display-popup", &["-E", "-w", width, "-h", height, command])
            .await
    }

    async fn select_pane(&self, pane: &str) -> Result<(), TmuxError> {
        self.run_silent("select-pane", &["-t", pane]).await
    }

    async fn capture_pane(&self, pane: &str) -> Result<Vec<String>, TmuxError> {
        let output = self.run("capture-pane", &["-t", pane, "-p"]).await?;
        // Trim trailing blank lines
        let mut lines: Vec<String> = output.lines().map(|l| l.to_string()).collect();
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        Ok(lines)
    }

    async fn new_session(&self, name: &str) -> Result<String, TmuxError> {
        let output = self
            .run("new-session", &["-d", "-s", name, "-P", "-F", "#{pane_id}"])
            .await?;
        let pane_id = output.trim().to_string();
        if pane_id.is_empty() {
            return Err(TmuxError::ParseError(
                "new-session returned empty pane ID".to_string(),
            ));
        }
        debug!(%pane_id, "new-session created");
        Ok(pane_id)
    }

    async fn get_pane_cwd(&self, pane: &str) -> Result<Option<String>, TmuxError> {
        let output = self
            .run(
                "display-message",
                &["-p", "-t", pane, "#{pane_current_path}"],
            )
            .await?;
        let path = output.trim().to_string();
        if path.is_empty() {
            Ok(None)
        } else {
            Ok(Some(path))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RAII guard that restores an env var's prior value on drop, even
    /// during unwinding. Necessary because `RealTmuxClient::new()` reads
    /// `TMUX_SOCKET_ENV` from process-global state — a panic mid-test
    /// would otherwise leak the mutated value into other tests.
    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                prev: std::env::var_os(key),
            }
        }
        fn set(&self, value: &str) {
            std::env::set_var(self.key, value);
        }
        fn unset(&self) {
            std::env::remove_var(self.key);
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn paste_buffer_names_differ_per_call() {
        let first = unique_buffer_name();
        let second = unique_buffer_name();
        assert_ne!(first, second);
        let prefix = format!("atm-send-{}-", std::process::id());
        assert!(first.starts_with(&prefix), "{first}");
        assert!(second.starts_with(&prefix), "{second}");
        // A pid can repeat across PID namespaces sharing one tmux server,
        // so the name also carries the time: atm-send-<pid>-<nanos>-<n>.
        let parts: Vec<&str> = first.split('-').collect();
        assert_eq!(parts.len(), 5, "{first}");
        assert!(
            parts.iter().skip(2).all(|p| p.parse::<u128>().is_ok()),
            "{first}"
        );
    }

    #[test]
    fn test_real_client_explicit_default_has_no_socket() {
        // `default()` is deterministic — unlike `new()`, it never reads
        // env vars. Use it whenever a test wants the no-socket variant.
        let client = RealTmuxClient::default();
        assert!(client.socket_name.is_none());
    }

    #[test]
    fn test_real_client_with_socket() {
        let client = RealTmuxClient::with_socket("test-server");
        assert_eq!(client.socket_name.as_deref(), Some("test-server"));
    }

    /// All env-var cases for `new()` live in one `#[test]` so they don't
    /// race each other under parallel test execution.
    #[test]
    fn test_real_client_new_honors_env() {
        let env = EnvGuard::capture(TMUX_SOCKET_ENV);

        env.set("custom-server");
        assert_eq!(
            RealTmuxClient::new().socket_name.as_deref(),
            Some("custom-server"),
            "TMUX_SOCKET_ENV value should be picked up"
        );

        env.set("");
        assert!(
            RealTmuxClient::new().socket_name.is_none(),
            "empty env should fall through to default"
        );

        env.unset();
        assert!(
            RealTmuxClient::new().socket_name.is_none(),
            "unset env should give the no-socket default"
        );
    }
}
