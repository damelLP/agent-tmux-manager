//! Tmux integration for the ATM TUI.
//!
//! Provides functions for:
//! - Checking if we're running inside tmux
//! - Jumping to a specific tmux pane
//!
//! # Panic-Free Guarantees
//!
//! This module follows CLAUDE.md panic-free policy:
//! - No `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, `todo!()`
//! - All fallible operations use `?`, pattern matching, or `unwrap_or`
//! - Tmux command failures are returned as errors

use std::process::Command;
use thiserror::Error;

// ============================================================================
// Error Types
// ============================================================================

/// Errors that can occur during tmux operations.
#[derive(Debug, Error)]
pub enum TmuxError {
    /// Not running inside tmux
    #[error("not running inside tmux")]
    NotInTmux,

    /// Failed to execute tmux command
    #[error("tmux command failed: {0}")]
    CommandFailed(String),

    /// Invalid pane ID
    #[error("invalid pane ID: {0}")]
    InvalidPaneId(String),
}

// ============================================================================
// Public Functions
// ============================================================================

/// Checks if we're running inside a tmux session.
///
/// Returns `true` if the `TMUX` environment variable is set,
/// indicating we're running inside tmux.
#[must_use]
pub fn is_in_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

/// Jumps to the specified tmux pane.
///
/// This handles panes in different windows and sessions by:
/// 1. Looking up the pane's session and window
/// 2. Switching to that session with `switch-client`
/// 3. Selecting the window with `select-window`
/// 4. Selecting the pane with `select-pane`
///
/// # Arguments
///
/// * `pane_id` - The tmux pane ID (e.g., "%5", "%12")
///
/// # Errors
///
/// * `TmuxError::NotInTmux` - If not running inside tmux
/// * `TmuxError::InvalidPaneId` - If the pane ID is empty
/// * `TmuxError::CommandFailed` - If the tmux command fails
///
/// # Example
///
/// ```ignore
/// use atm_tui::tmux::jump_to_pane;
///
/// // Jump to pane %5
/// jump_to_pane("%5")?;
/// ```
pub fn jump_to_pane(pane_id: &str) -> Result<(), TmuxError> {
    // Validate we're in tmux
    if !is_in_tmux() {
        return Err(TmuxError::NotInTmux);
    }

    // Validate pane ID
    if pane_id.is_empty() {
        return Err(TmuxError::InvalidPaneId(pane_id.to_string()));
    }

    // Step 1: Get the pane's session and window information
    // Format: "pane_id session_name window_id"
    let list_output = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id} #{session_name} #{window_id}",
        ])
        .output()
        .map_err(|e| TmuxError::CommandFailed(e.to_string()))?;

    if !list_output.status.success() {
        let stderr = String::from_utf8_lossy(&list_output.stderr);
        return Err(TmuxError::CommandFailed(format!(
            "list-panes failed: {}",
            stderr.trim()
        )));
    }

    // Find the line matching our pane_id
    let output_str = String::from_utf8_lossy(&list_output.stdout);
    let pane_info = output_str
        .lines()
        .find(|line| line.starts_with(pane_id))
        .ok_or_else(|| {
            TmuxError::CommandFailed(format!("pane {pane_id} not found in any session"))
        })?;

    // Parse: "%5 session_name @3"
    let parts: Vec<&str> = pane_info.split_whitespace().collect();
    if parts.len() < 3 {
        return Err(TmuxError::CommandFailed(format!(
            "unexpected pane info format: {pane_info}"
        )));
    }
    let session_name = parts[1];
    let window_id = parts[2];

    // Step 2: Switch to the session
    let _ = Command::new("tmux")
        .args(["switch-client", "-t", session_name])
        .output();

    // Step 3: Select the window
    let _ = Command::new("tmux")
        .args(["select-window", "-t", window_id])
        .output();

    // Step 4: Select the pane
    let output = Command::new("tmux")
        .args(["select-pane", "-t", pane_id])
        .output()
        .map_err(|e| TmuxError::CommandFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TmuxError::CommandFailed(format!(
            "select-pane failed: {}",
            stderr.trim()
        )));
    }

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_in_tmux_returns_bool() {
        // Just verify it doesn't panic and returns a bool
        let _ = is_in_tmux();
    }

    #[test]
    fn test_jump_to_pane_empty_pane_id() {
        let result = jump_to_pane("");
        assert!(matches!(result, Err(TmuxError::InvalidPaneId(_))));
    }

    #[test]
    fn test_tmux_error_display() {
        let err = TmuxError::NotInTmux;
        assert_eq!(err.to_string(), "not running inside tmux");

        let err = TmuxError::CommandFailed("test error".to_string());
        assert_eq!(err.to_string(), "tmux command failed: test error");

        let err = TmuxError::InvalidPaneId("".to_string());
        assert_eq!(err.to_string(), "invalid pane ID: ");
    }
}
