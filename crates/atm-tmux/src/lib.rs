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
pub mod mock;

pub use client::RealTmuxClient;
pub use error::TmuxError;
pub use mock::MockTmuxClient;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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
    /// * `horizontal` — If true, split horizontally (top/bottom); if false, vertically (left/right).
    /// * `command` — Optional command to run in the new pane.
    async fn split_window(
        &self,
        target: &str,
        size: &str,
        horizontal: bool,
        command: Option<&str>,
    ) -> Result<String, TmuxError>;

    /// Creates a new window in the given session, returning the new pane ID.
    async fn new_window(
        &self,
        session: &str,
        command: Option<&str>,
    ) -> Result<String, TmuxError>;

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
}
