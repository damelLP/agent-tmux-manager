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
/// `switch-client -t <pane_id>` moves the current client to the pane's
/// session and window and makes the pane active, in one command. tmux
/// resolves the target, so grouped sessions keep the client in its own
/// session (checked on tmux 3.8, including from a `display-popup`).
/// A pane as the `switch-client` target requires tmux 1.9+.
///
/// # Arguments
///
/// * `client` - The tmux client to run the command through
/// * `pane_id` - The tmux pane ID (e.g., "%5", "%12")
///
/// # Errors
///
/// * `TmuxError::NotInTmux` - If not running inside tmux
/// * `TmuxError::InvalidPaneId` - If the pane ID is empty
/// * `TmuxError::Tmux` - If tmux fails (e.g. the pane is gone) or times out
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

    client
        .raw_command("switch-client", &["-t", pane_id])
        .await?;

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

        // Outside tmux: refuse before touching tmux at all.
        std::env::remove_var("TMUX");
        let mock = MockTmuxClient::new();
        let result = jump_to_pane(&mock, "%5").await;
        assert!(matches!(result, Err(TmuxError::NotInTmux)), "{result:?}");
        assert_eq!(mock.call_count(), 0);

        std::env::set_var("TMUX", "/tmp/tmux-test/default,1,0");

        // One command, targeting the exact pane id: tmux resolves the
        // session and window itself. No `list-panes` output to misparse
        // ("%1" vs "%10", spaces in session names, grouped sessions).
        let mock = MockTmuxClient::new();
        jump_to_pane(&mock, "%1")
            .await
            .expect("jump should succeed");
        assert_eq!(mock.calls(), vec![raw("switch-client", &["-t", "%1"])]);

        // tmux errors (e.g. the pane is gone) and timeouts are returned as-is.
        for err in [
            atm_tmux::TmuxError::CommandFailed {
                command: "switch-client".to_string(),
                stderr: "can't find pane: %5".to_string(),
            },
            atm_tmux::TmuxError::Timeout {
                command: "switch-client".to_string(),
                timeout: std::time::Duration::from_secs(5),
            },
        ] {
            let mock = MockTmuxClient::new();
            mock.set_next_error(err);
            let result = jump_to_pane(&mock, "%5").await;
            assert!(matches!(result, Err(TmuxError::Tmux(_))), "{result:?}");
            assert_eq!(mock.call_count(), 1);
        }
    }

    #[test]
    fn test_tmux_error_display() {
        let err = TmuxError::NotInTmux;
        assert_eq!(err.to_string(), "not running inside tmux");

        let err = TmuxError::Tmux(atm_tmux::TmuxError::CommandFailed {
            command: "switch-client".to_string(),
            stderr: "can't find pane: %5".to_string(),
        });
        assert_eq!(
            err.to_string(),
            "tmux command failed: switch-client — can't find pane: %5"
        );

        let err = TmuxError::InvalidPaneId("".to_string());
        assert_eq!(err.to_string(), "invalid pane ID: ");
    }
}
