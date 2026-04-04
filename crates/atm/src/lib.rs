//! ATM TUI - Library modules
//!
//! This library provides the core TUI components for managing Claude Code sessions.
//!
//! # Architecture
//!
//! The TUI uses an event-driven architecture with three main components:
//!
//! 1. **Keyboard Task**: Polls for keyboard input and sends events to the main loop
//! 2. **Daemon Client Task**: Maintains connection to the daemon and forwards session updates
//! 3. **Main Event Loop**: Processes events, updates state, and renders the UI
//!
//! All tasks respect a shared `CancellationToken` for graceful shutdown.

pub mod app;
pub mod client;
pub mod daemon;
pub mod error;
pub mod input;
pub mod keybinding;
pub mod setup;
pub mod tmux;
pub mod ui;

// Re-export commonly used types
pub use app::App;
pub use client::DaemonClient;
pub use error::{Result, TuiError};
