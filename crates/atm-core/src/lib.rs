//! ATM Core - Shared types for coding-agent management
//!
//! This crate provides the core domain types shared between
//! the daemon (atmd) and TUI (atm).
//!
//! All code follows the panic-free policy: no `.unwrap()`, `.expect()`,
//! `panic!()`, `unreachable!()`, `todo!()`, or direct indexing `[i]`.

pub mod agent;
pub mod beads;
pub mod context;
pub mod cost;
pub mod error;
pub mod harness;
pub mod harness_registry;
pub mod lifecycle;
pub mod model;
pub mod project;
pub mod session;
pub mod tool;
pub mod tree;

// Re-exports for convenience
pub use agent::AgentType;
pub use context::{ContextUsage, TokenCount};
pub use cost::Money;
pub use error::{DomainError, DomainResult};
pub use harness::Harness;
pub use harness_registry::{
    builtin_harness_ids_display, builtin_harnesses, default_harness_definition,
    find_harness_definition, HarnessDefinition, ProcessMatcher, PromptMode,
};
pub use lifecycle::{LifecycleEvent, NeedsInputReason, NotificationKind};
pub use model::{derive_display_name, Model};
pub use project::{resolve_project_root, resolve_worktree_info};
pub use session::{
    ActivityDetail, LinesChanged, SessionDomain, SessionDuration, SessionId, SessionInfrastructure,
    SessionStatus, SessionView, StatusLineData, ToolUsageRecord, ToolUseId, TranscriptPath,
};
pub use tool::Tool;
pub use tree::{
    all_node_ids, build_tree, flatten_tree, TreeNode, TreeNodeId, TreeRow, TreeRowKind,
};
