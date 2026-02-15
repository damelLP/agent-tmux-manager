//! Event types and daemon communication for the ATM TUI.
//!
//! This module provides event types for keyboard input, terminal resizing,
//! and daemon communication. Keybinding logic lives in the `keybinding` module.
//!
//! All code follows the panic-free policy: no `.unwrap()`, `.expect()`,
//! `panic!()`, `unreachable!()`, `todo!()`, or direct indexing `[i]`.

use atm_core::SessionView;
use crossterm::event::KeyEvent;

// ============================================================================
// Event Types
// ============================================================================

/// Events that the TUI can receive and process.
///
/// These events drive the main event loop and include both user input
/// and system-generated events.
#[derive(Debug, Clone)]
pub enum Event {
    /// Keyboard input from the user.
    Key(KeyEvent),

    /// Terminal window resize event.
    Resize(u16, u16),

    /// Session data update received from the daemon (merge with existing).
    SessionUpdate(Vec<SessionView>),

    /// Full session list received from daemon (replace all sessions).
    SessionListReplace(Vec<SessionView>),

    /// Connection to the daemon was lost.
    DaemonDisconnected,

    /// A session was removed from the daemon.
    SessionRemoved(String),

    /// Discovery operation completed.
    DiscoveryComplete {
        /// Number of sessions discovered.
        discovered: u32,
        /// Number of failures during discovery.
        failed: u32,
    },
}

// ============================================================================
// Client Commands
// ============================================================================

/// Commands that can be sent to the daemon client from the main loop.
#[derive(Debug, Clone)]
pub enum ClientCommand {
    /// Request session discovery from the daemon.
    Discover,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn test_event_key_variant() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let event = Event::Key(key);
        match event {
            Event::Key(k) => assert_eq!(k.code, KeyCode::Enter),
            _ => panic!("Expected Key event"),
        }
    }

    #[test]
    fn test_event_resize_variant() {
        let event = Event::Resize(80, 24);
        match event {
            Event::Resize(w, h) => {
                assert_eq!(w, 80);
                assert_eq!(h, 24);
            }
            _ => panic!("Expected Resize event"),
        }
    }

    #[test]
    fn test_client_command_discover() {
        let cmd = ClientCommand::Discover;
        assert!(matches!(cmd, ClientCommand::Discover));
    }
}
