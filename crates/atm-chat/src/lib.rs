//! Pure terminal-chat state and rendering on canonical durable messages.
//!
//! The binary owns the dedicated connection, terminal, tmux and supplied clock.
//! This crate performs no I/O and never treats drawing as a canonical read fact.

mod screen;
mod state;
mod types;

pub use screen::render;
pub use state::ChatState;
pub use types::*;

/// Controller-only marker for the supported Watch envelope, not an RPC method.
pub const WATCH_CAPABILITY: &str = "watch";
