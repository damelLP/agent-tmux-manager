//! ATM Tmux — Thin async wrapper over the tmux CLI.
//!
//! Provides the [`TmuxClient`] trait for tmux pane management, with a real
//! implementation ([`RealTmuxClient`]) that shells out to `tmux` via
//! `tokio::process::Command`, and a mock ([`MockTmuxClient`]) for testing.
//!
//! All code follows the panic-free policy: no `.unwrap()`, `.expect()`,
//! `panic!()`, `unreachable!()`, `todo!()`, or direct indexing `[i]`.

pub mod client;
pub mod error;
pub mod layout;
pub mod mock;

pub use client::RealTmuxClient;
pub use error::TmuxError;
pub use mock::MockTmuxClient;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Direction for placing a new pane relative to the target pane.
///
/// Maps directly to tmux `split-window` flags:
/// - `Left`  → `-h -b` (horizontal split, new pane before/left)
/// - `Right` → `-h`    (horizontal split, new pane after/right)
/// - `Above` → `-v -b` (vertical split, new pane before/above)
/// - `Below` → `-v`    (vertical split, new pane after/below)
///
/// The `-b` (before) flag requires tmux 3.1+.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneDirection {
    Left,
    Right,
    Above,
    Below,
}

/// Information about a single tmux pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    /// Pane ID (e.g., "%5").
    pub pane_id: String,
    /// Session name this pane belongs to.
    pub session_name: String,
    /// Window index within the session.
    pub window_index: u32,
    /// PID of the shell process running in the pane.
    pub pane_pid: u32,
    /// Pane width in columns.
    pub width: u16,
    /// Pane height in rows.
    pub height: u16,
    /// Whether this pane is the currently active pane.
    pub is_active: bool,
}

/// Async interface for tmux pane management.
///
/// The real implementation shells out to the `tmux` CLI. The mock records
/// calls for test assertions.
#[async_trait]
pub trait TmuxClient: Send + Sync {
    /// Splits a pane, returning the new pane ID (e.g., "%7").
    ///
    /// # Arguments
    /// * `target` — Target pane to split (e.g., "%5").
    /// * `size` — Size specification (e.g., "30%", "20").
    /// * `direction` — Where to place the new pane relative to the target.
    /// * `command` — Optional command to run in the new pane.
    async fn split_window(
        &self,
        target: &str,
        size: &str,
        direction: PaneDirection,
        command: Option<&str>,
    ) -> Result<String, TmuxError>;

    /// Returns the current working directory of a pane, or `None` if unavailable.
    ///
    /// Queries tmux's `#{pane_current_path}` format variable.
    async fn get_pane_cwd(&self, pane: &str) -> Result<Option<String>, TmuxError>;

    /// Creates a new window in the given session, returning the new pane ID.
    async fn new_window(&self, session: &str, command: Option<&str>) -> Result<String, TmuxError>;

    /// Kills (closes) a pane.
    async fn kill_pane(&self, pane: &str) -> Result<(), TmuxError>;

    /// Resizes a pane.
    ///
    /// At least one of `width` or `height` must be `Some`.
    async fn resize_pane(
        &self,
        pane: &str,
        width: Option<u16>,
        height: Option<u16>,
    ) -> Result<(), TmuxError>;

    /// Sends keystrokes to a pane.
    async fn send_keys(&self, pane: &str, keys: &str) -> Result<(), TmuxError>;

    /// Lists all panes across all sessions.
    async fn list_panes(&self) -> Result<Vec<PaneInfo>, TmuxError>;

    /// Displays a popup overlay.
    async fn display_popup(
        &self,
        width: &str,
        height: &str,
        command: &str,
    ) -> Result<(), TmuxError>;

    /// Selects (focuses) a pane.
    async fn select_pane(&self, pane: &str) -> Result<(), TmuxError>;

    /// Captures the visible content of a pane.
    ///
    /// Returns the text currently displayed in the pane, one string per line.
    /// Trailing blank lines are trimmed.
    async fn capture_pane(&self, pane: &str) -> Result<Vec<String>, TmuxError>;

    /// Creates a new detached tmux session, returning the initial pane ID.
    async fn new_session(&self, name: &str) -> Result<String, TmuxError>;
}
