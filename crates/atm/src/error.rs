//! Error types for the ATM TUI.
//!
//! This module defines TUI-specific errors that can occur during terminal
//! initialization, daemon communication, and UI rendering.
//!
//! All error types use `thiserror` for derive macros and provide clear,
//! user-friendly error messages with actionable suggestions.
//!
//! **Panic-Free Policy:** This module follows the project's panic-free guidelines.
//! No `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, or `todo!()`.

use std::io;
use thiserror::Error;

// ============================================================================
// TUI Error Type
// ============================================================================

/// TUI application errors.
///
/// Represents all errors that can occur in the ATM TUI application,
/// from terminal initialization through daemon communication to UI rendering.
///
/// # Error Handling
///
/// Most errors include actionable information for the user:
/// - Connection errors suggest checking if the daemon is running
/// - Terminal errors suggest checking terminal compatibility
/// - Protocol errors may indicate version mismatches
///
/// # Example
///
/// ```rust,ignore
/// use atm_tui::error::{TuiError, Result};
///
/// fn connect_to_daemon() -> Result<()> {
///     // Connection logic that may fail
///     Err(TuiError::DaemonConnection("connection refused".to_string()))
/// }
/// ```
#[derive(Error, Debug)]
pub enum TuiError {
    /// Failed to initialize the terminal.
    ///
    /// This occurs when the TUI cannot set up raw mode, alternate screen,
    /// or other terminal requirements. Common causes include:
    /// - Running in a non-TTY environment (pipes, scripts)
    /// - Unsupported terminal emulator
    /// - Permission issues
    #[error("Failed to initialize terminal: {0}")]
    TerminalInit(String),

    /// Failed to cleanup/restore the terminal.
    ///
    /// This occurs when the TUI cannot restore the terminal to its
    /// original state on exit. The terminal may be left in an
    /// inconsistent state; running `reset` can help recover.
    #[error("Failed to restore terminal: {0}")]
    TerminalCleanup(String),

    /// Failed to connect to the daemon.
    ///
    /// This is a general connection error that includes the reason.
    #[error("Failed to connect to daemon: {0}")]
    DaemonConnection(String),

    /// Protocol version mismatch with daemon
    ///
    /// The TUI and daemon are running incompatible protocol versions.
    /// This typically happens when either the TUI or daemon has been
    /// updated but not the other. Ensure both are the same version.
    #[error("Protocol version mismatch (client: {client_version}, daemon: {daemon_version})")]
    VersionMismatch {
        /// The protocol version the client (TUI) supports.
        client_version: String,
        /// The protocol version the daemon is running.
        daemon_version: String,
    },

    /// The daemon answered `Connect` with `Rejected`, giving this reason
    /// (e.g. an incompatible protocol version).
    #[error("Daemon rejected the connection: {0}")]
    Rejected(String),

    /// Protocol parse or format error.
    ///
    /// A message from the daemon could not be parsed or a message
    /// to the daemon could not be formatted. This may indicate
    /// a version mismatch between the TUI and daemon.
    #[error("Protocol error: {0}")]
    ProtocolError(String),

    /// I/O error passthrough.
    ///
    /// A low-level I/O error occurred during socket or terminal operations.
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// JSON parse error passthrough.
    ///
    /// Failed to parse JSON data from the daemon.
    /// This typically indicates a protocol error or version mismatch.
    #[error("Failed to parse message: {0}")]
    ParseError(#[from] serde_json::Error),
}

// ============================================================================
// Result Type Alias
// ============================================================================

/// Convenience Result type alias for TUI operations.
///
/// # Example
///
/// ```rust,ignore
/// use atm_tui::error::Result;
///
/// fn initialize_terminal() -> Result<()> {
///     // Terminal setup that may fail
///     Ok(())
/// }
/// ```
pub type Result<T> = std::result::Result<T, TuiError>;

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_terminal_init_error_display() {
        let error = TuiError::TerminalInit("not a TTY".to_string());
        let display = format!("{error}");
        assert!(display.contains("Failed to initialize terminal"));
        assert!(display.contains("not a TTY"));
    }

    #[test]
    fn test_terminal_cleanup_error_display() {
        let error = TuiError::TerminalCleanup("could not restore cursor".to_string());
        let display = format!("{error}");
        assert!(display.contains("Failed to restore terminal"));
        assert!(display.contains("could not restore cursor"));
    }

    #[test]
    fn test_daemon_connection_error_display() {
        let error = TuiError::DaemonConnection("refused".to_string());
        let display = format!("{error}");
        assert!(display.contains("Failed to connect to daemon"));
        assert!(display.contains("refused"));
    }

    #[test]
    fn test_version_mismatch_error_display() {
        let error = TuiError::VersionMismatch {
            client_version: "1.0.0".to_string(),
            daemon_version: "2.0.0".to_string(),
        };
        let display = format!("{error}");
        assert!(display.contains("Protocol version mismatch"));
        assert!(display.contains("client: 1.0.0"));
        assert!(display.contains("daemon: 2.0.0"));
    }

    #[test]
    fn test_protocol_error_display() {
        let error = TuiError::ProtocolError("invalid message type".to_string());
        let display = format!("{error}");
        assert!(display.contains("Protocol error"));
        assert!(display.contains("invalid message type"));
    }

    #[test]
    fn test_io_error_from_conversion() {
        let io_error = io::Error::new(io::ErrorKind::NotFound, "socket not found");
        let tui_error: TuiError = io_error.into();
        assert!(matches!(tui_error, TuiError::Io(_)));
        let display = format!("{tui_error}");
        assert!(display.contains("IO error"));
    }

    #[test]
    fn test_parse_error_from_conversion() {
        let json_str = "{ invalid json }";
        let parse_result: std::result::Result<serde_json::Value, _> =
            serde_json::from_str(json_str);
        let json_error = parse_result.unwrap_err();
        let tui_error: TuiError = json_error.into();
        assert!(matches!(tui_error, TuiError::ParseError(_)));
        let display = format!("{tui_error}");
        assert!(display.contains("Failed to parse message"));
    }

    #[test]
    fn test_error_debug_impl() {
        let error = TuiError::DaemonConnection("test".to_string());
        // Debug impl should not panic
        let debug = format!("{error:?}");
        assert!(debug.contains("DaemonConnection"));
    }

    #[test]
    fn test_result_type_alias_ok() {
        fn returns_ok() -> Result<i32> {
            Ok(42)
        }
        assert_eq!(returns_ok().unwrap(), 42);
    }

    #[test]
    fn test_result_type_alias_err() {
        fn returns_err() -> Result<i32> {
            Err(TuiError::DaemonConnection("test".to_string()))
        }
        assert!(returns_err().is_err());
    }
}
