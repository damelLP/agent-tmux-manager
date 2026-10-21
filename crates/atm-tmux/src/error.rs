//! Error types for tmux operations.

use thiserror::Error;

/// Errors that can occur during tmux CLI operations.
#[derive(Debug, Error)]
pub enum TmuxError {
    /// A tmux command exited with non-zero status.
    #[error("tmux command failed: {command} — {stderr}")]
    CommandFailed {
        /// The tmux subcommand that failed (e.g., "split-window").
        command: String,
        /// Stderr output from the failed command.
        stderr: String,
    },

    /// A tmux command did not finish before its deadline (e.g. the tmux
    /// server is stopped or wedged). The `tmux` process has been killed.
    #[error("tmux command timed out: {command}")]
    Timeout {
        /// The tmux subcommand that timed out (e.g., "list-panes").
        command: String,
    },

    /// tmux binary not found in PATH.
    #[error("tmux not found in PATH")]
    NotFound,

    /// Failed to parse tmux output.
    #[error("failed to parse tmux output: {0}")]
    ParseError(String),

    /// The specified pane was not found.
    #[error("pane not found: {0}")]
    PaneNotFound(String),

    /// An I/O error occurred when spawning or communicating with tmux.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
