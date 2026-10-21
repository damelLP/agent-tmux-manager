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

use atm_tmux::TmuxClient;
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

    /// The tmux client failed (non-zero exit, timeout, tmux missing, ...)
    #[error(transparent)]
    Tmux(#[from] atm_tmux::TmuxError),
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
/// * `client` - The tmux client to run the commands through
/// * `pane_id` - The tmux pane ID (e.g., "%5", "%12")
///
/// # Errors
///
/// * `TmuxError::NotInTmux` - If not running inside tmux
/// * `TmuxError::InvalidPaneId` - If the pane ID is empty
/// * `TmuxError::CommandFailed` - If the pane is not listed by tmux
/// * `TmuxError::Tmux` - If the lookup or `select-pane` fails or times out
///
/// # Example
///
/// ```ignore
/// use atm_tmux::RealTmuxClient;
/// use atm_tui::tmux::jump_to_pane;
///
/// // Jump to pane %5
/// jump_to_pane(&RealTmuxClient::new(), "%5").await?;
/// ```
pub async fn jump_to_pane<C>(client: &C, pane_id: &str) -> Result<(), TmuxError>
where
    C: TmuxClient + ?Sized,
{
    // Validate pane ID before checking environment — an empty pane ID is
    // invalid regardless of whether tmux is running.
    if pane_id.is_empty() {
        return Err(TmuxError::InvalidPaneId(pane_id.to_string()));
    }

    // Validate we're in tmux
    if !is_in_tmux() {
        return Err(TmuxError::NotInTmux);
    }

    // Step 1: Get the pane's session and window information
    // Format: "pane_id session_name window_id"
    let output_str = client
        .raw_command(
            "list-panes",
            &["-a", "-F", "#{pane_id} #{session_name} #{window_id}"],
        )
        .await?;

    // Find the line matching our pane_id
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
    let _ = client
        .raw_command("switch-client", &["-t", session_name])
        .await;

    // Step 3: Select the window
    let _ = client
        .raw_command("select-window", &["-t", window_id])
        .await;

    // Step 4: Select the pane
    client.select_pane(pane_id).await?;

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use atm_tmux::mock::MockCall;
    use atm_tmux::MockTmuxClient;

    #[test]
    fn test_is_in_tmux_returns_bool() {
        // Just verify it doesn't panic and returns a bool
        let _ = is_in_tmux();
    }

    #[tokio::test]
    async fn test_jump_to_pane_empty_pane_id() {
        let mock = MockTmuxClient::new();
        let result = jump_to_pane(&mock, "").await;
        assert!(matches!(result, Err(TmuxError::InvalidPaneId(_))));
        assert_eq!(mock.call_count(), 0);
    }

    /// Restores `TMUX` to its prior value on drop, even during unwinding,
    /// so a failing case can't leak env state into other tests.
    struct TmuxEnvGuard {
        prev: Option<std::ffi::OsString>,
    }

    impl Drop for TmuxEnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("TMUX", v),
                None => std::env::remove_var("TMUX"),
            }
        }
    }

    fn raw(subcommand: &str, args: &[&str]) -> MockCall {
        MockCall::RawCommand {
            subcommand: subcommand.to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
        }
    }

    /// All `TMUX`-dependent cases live in one test so they don't race each
    /// other under parallel test execution.
    #[tokio::test]
    async fn test_jump_to_pane_via_client() {
        let _env = TmuxEnvGuard {
            prev: std::env::var_os("TMUX"),
        };
        let list_panes = raw(
            "list-panes",
            &["-a", "-F", "#{pane_id} #{session_name} #{window_id}"],
        );

        // Outside tmux: refuse before touching tmux at all.
        std::env::remove_var("TMUX");
        let mock = MockTmuxClient::new();
        let result = jump_to_pane(&mock, "%5").await;
        assert!(matches!(result, Err(TmuxError::NotInTmux)), "{result:?}");
        assert_eq!(mock.call_count(), 0);

        std::env::set_var("TMUX", "/tmp/tmux-test/default,1,0");

        // Found: switch session, then window, then pane.
        let mock = MockTmuxClient::new();
        mock.set_next_raw_output("%3 work @2\n%5 main @7\n");
        jump_to_pane(&mock, "%5")
            .await
            .expect("jump should succeed");
        assert_eq!(
            mock.calls(),
            vec![
                list_panes.clone(),
                raw("switch-client", &["-t", "main"]),
                raw("select-window", &["-t", "@7"]),
                MockCall::SelectPane {
                    pane: "%5".to_string()
                },
            ]
        );

        // Not listed: error, and no switching.
        let mock = MockTmuxClient::new();
        mock.set_next_raw_output("%3 work @2\n");
        let result = jump_to_pane(&mock, "%5").await;
        assert!(
            matches!(result, Err(TmuxError::CommandFailed(_))),
            "{result:?}"
        );
        assert_eq!(mock.calls(), vec![list_panes.clone()]);

        // Lookup times out: the client's error is returned as-is.
        let mock = MockTmuxClient::new();
        mock.set_next_error(atm_tmux::TmuxError::Timeout {
            command: "list-panes".to_string(),
        });
        let result = jump_to_pane(&mock, "%5").await;
        assert!(
            matches!(
                result,
                Err(TmuxError::Tmux(atm_tmux::TmuxError::Timeout { .. }))
            ),
            "{result:?}"
        );
        assert_eq!(mock.calls(), vec![list_panes]);
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
