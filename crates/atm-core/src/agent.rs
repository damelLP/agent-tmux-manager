//! Agent type identification.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Type of Claude Code agent.
///
/// Claude Code spawns different agent types for different purposes:
/// - Main agent for general tasks
/// - Specialized subagents for exploration, planning, code review
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum AgentType {
    /// General-purpose main agent
    #[default]
    GeneralPurpose,

    /// Task/explore subagent for file exploration
    Explore,

    /// Planning subagent for task breakdown
    Plan,

    /// Code review subagent
    CodeReviewer,

    /// File search/analysis subagent
    FileSearch,

    /// In-process child spawned by a parent session with no more
    /// specific role (Claude `Agent` tool).
    Subagent,

    /// Named member of an agent team, addressed by name under its lead.
    Teammate,

    /// Custom or unknown agent type
    Custom(String),
}

impl AgentType {
    /// Returns a short identifier for display.
    pub fn short_name(&self) -> &str {
        match self {
            Self::GeneralPurpose => "main",
            Self::Explore => "explore",
            Self::Plan => "plan",
            Self::CodeReviewer => "review",
            Self::FileSearch => "search",
            Self::Subagent => "subagent",
            Self::Teammate => "teammate",
            Self::Custom(name) => name.as_str(),
        }
    }

    /// Returns a descriptive label for the agent type.
    pub fn label(&self) -> &str {
        match self {
            Self::GeneralPurpose => "General Purpose",
            Self::Explore => "Explorer",
            Self::Plan => "Planner",
            Self::CodeReviewer => "Code Reviewer",
            Self::FileSearch => "File Search",
            Self::Subagent => "Subagent",
            Self::Teammate => "Teammate",
            Self::Custom(_) => "Custom",
        }
    }

    /// Parses an agent type from a subagent_type string.
    pub fn from_subagent_type(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "general-purpose" | "general_purpose" => Self::GeneralPurpose,
            "explore" | "explorer" => Self::Explore,
            "plan" | "planner" => Self::Plan,
            "code-reviewer" | "code_reviewer" | "codereview" => Self::CodeReviewer,
            "file-search" | "file_search" | "filesearch" => Self::FileSearch,
            "subagent" => Self::Subagent,
            "teammate" => Self::Teammate,
            _ => Self::Custom(s.to_string()),
        }
    }

    /// Type for an in-process child described by a vendor `role`.
    ///
    /// A `general-purpose` child is a plain [`Subagent`](Self::Subagent):
    /// `GeneralPurpose` means a main session, and a child must not be
    /// mistaken for one.
    #[must_use]
    pub fn for_child(role: Option<&str>) -> Self {
        match role.map(str::trim).filter(|role| !role.is_empty()) {
            None => Self::Subagent,
            Some(role) => match Self::from_subagent_type(role) {
                Self::GeneralPurpose => Self::Subagent,
                agent_type => agent_type,
            },
        }
    }
}

impl fmt::Display for AgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_type_parsing() {
        assert_eq!(
            AgentType::from_subagent_type("general-purpose"),
            AgentType::GeneralPurpose
        );
        assert_eq!(AgentType::from_subagent_type("explore"), AgentType::Explore);
        assert_eq!(
            AgentType::from_subagent_type("custom-agent"),
            AgentType::Custom("custom-agent".to_string())
        );
    }

    #[test]
    fn child_role_never_yields_a_main_agent() {
        assert_eq!(AgentType::for_child(None), AgentType::Subagent);
        assert_eq!(AgentType::for_child(Some("  ")), AgentType::Subagent);
        assert_eq!(
            AgentType::for_child(Some("general-purpose")),
            AgentType::Subagent
        );
        assert_eq!(AgentType::for_child(Some("teammate")), AgentType::Teammate);
        assert_eq!(AgentType::for_child(Some("explore")), AgentType::Explore);
        assert_eq!(
            AgentType::for_child(Some("triage")),
            AgentType::Custom("triage".to_string())
        );
    }

    #[test]
    fn test_agent_type_short_name() {
        assert_eq!(AgentType::GeneralPurpose.short_name(), "main");
        assert_eq!(AgentType::Explore.short_name(), "explore");
    }
}
