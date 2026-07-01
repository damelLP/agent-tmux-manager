//! Real tmux client implementation.
//!
//! Shells out to the `tmux` binary via `tokio::process::Command`.

use async_trait::async_trait;
use tokio::process::Command;
use tracing::{debug, trace};

use crate::{PaneDirection, PaneInfo, TmuxClient, TmuxError};

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
    async fn run(&self, subcommand: &str, args: &[&str]) -> Result<String, TmuxError> {
        trace!(subcommand, ?args, "running tmux command");

        let output = self
            .tmux_cmd()
            .arg(subcommand)
            .args(args)
            .output()
            .await
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
