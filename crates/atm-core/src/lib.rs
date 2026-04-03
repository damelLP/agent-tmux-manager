//! ATM Core - Shared types for Claude Code monitoring
//!
//! This crate provides the core domain types shared between
//! the daemon (atmd) and TUI (atm).
//!
//! All code follows the panic-free policy: no `.unwrap()`, `.expect()`,
//! `panic!()`, `unreachable!()`, `todo!()`, or direct indexing `[i]`.

pub mod agent;
pub mod context;
pub mod cost;
pub mod error;
pub mod hook;
pub mod model;
pub mod project;
pub mod session;

// Re-exports for convenience
pub use agent::AgentType;
pub use context::{ContextUsage, TokenCount};
pub use cost::Money;
pub use error::{DomainError, DomainResult};
pub use hook::{is_interactive_tool, HookEventType};
pub use model::{derive_display_name, Model};
pub use project::{resolve_project_root, resolve_worktree_info};
pub use session::{
    ActivityDetail, LinesChanged, SessionDomain, SessionDuration, SessionId, SessionInfrastructure,
    SessionStatus, SessionView, StatusLineData, ToolUsageRecord, ToolUseId, TranscriptPath,
};
