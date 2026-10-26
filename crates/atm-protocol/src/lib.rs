//! ATM Protocol - Wire protocol for daemon communication
//!
//! This crate provides vendor-neutral message types and parsing for
//! communication between hook/extension scripts and the daemon, and
//! between the daemon and TUI clients. Vendor-specific wire payloads
//! (Claude `RawHookEvent` and `RawStatusLine`, pi `RawPiEvent`) live in
//! their respective adapter crates.

pub mod message;
pub mod version;

pub use message::{ClientMessage, DaemonMessage, MessageType};
pub use version::ProtocolVersion;
