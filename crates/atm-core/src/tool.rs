//! Vendor-neutral identity for tools an agent can invoke.
//!
//! Tool names are an *open set* — Claude has built-in tools, MCP plugins
//! generate `mcp__<server>__<name>` at runtime, and each vendor adds its
//! own. So the enum is closed for **well-known** tools (those we
//! special-case for permission gating or display) and falls through to
//! `Other(String)` for everything else.
//!
//! Wire format: serializes as the bare tool-name string (e.g. `"Bash"`,
//! `"AskUserQuestion"`, `"mcp__github__list_issues"`). The
//! `serde(into/from = "String")` attribute makes this transparent — the
//! enum is purely an internal representation.

use serde::{Deserialize, Serialize};

/// A tool an agent can invoke.
///
/// Variants are limited to tools the daemon special-cases (interactive
/// permission-gated tools, common tools we render with a friendlier
/// label). Everything else lives in `Other(String)` and round-trips
/// losslessly.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub enum Tool {
    // === Interactive (Claude permission-gated) ===
    /// `AskUserQuestion` — pauses and prompts the user.
    AskUserQuestion,
    /// `EnterPlanMode` — pauses for plan approval.
    EnterPlanMode,
    /// `ExitPlanMode` — pauses presenting the plan.
    ExitPlanMode,

    // === Common — recognized so the UI gets a stable label ===
    Bash,
    Read,
    Write,
    Edit,
    Grep,
    Glob,
    Task,
    WebSearch,
    WebFetch,
    TodoWrite,
    NotebookEdit,
    NotebookRead,

    /// Any other tool name — MCP tools, vendor-specific tools, future
    /// well-known tools we haven't promoted to a variant yet.
    Other(String),
}

impl Tool {
    /// True for tools whose `PreToolUse` event means the session is
    /// awaiting user input rather than running.
    ///
    /// Adapter authors: this classification is Claude-driven today (pi
    /// has no equivalent event — its permission gating is extension-
    /// mediated and surfaces via `NeedsInputReason::PermissionGate`).
    /// If a vendor's tool genuinely blocks waiting on user input,
    /// promote it to a variant rather than relying on string matching.
    #[must_use]
    pub fn is_interactive(&self) -> bool {
        matches!(
            self,
            Self::AskUserQuestion | Self::EnterPlanMode | Self::ExitPlanMode
        )
    }

    /// Canonical wire-format string for this tool.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::AskUserQuestion => "AskUserQuestion",
            Self::EnterPlanMode => "EnterPlanMode",
            Self::ExitPlanMode => "ExitPlanMode",
            Self::Bash => "Bash",
            Self::Read => "Read",
            Self::Write => "Write",
            Self::Edit => "Edit",
            Self::Grep => "Grep",
            Self::Glob => "Glob",
            Self::Task => "Task",
            Self::WebSearch => "WebSearch",
            Self::WebFetch => "WebFetch",
            Self::TodoWrite => "TodoWrite",
            Self::NotebookEdit => "NotebookEdit",
            Self::NotebookRead => "NotebookRead",
            Self::Other(s) => s.as_str(),
        }
    }
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Tool {
    /// Canonical lookup for known variants. Returns `None` for anything
    /// that should fall through to `Other(_)`. Input should already be
    /// trimmed by the caller.
    fn try_from_known(s: &str) -> Option<Self> {
        Some(match s {
            "AskUserQuestion" => Self::AskUserQuestion,
            "EnterPlanMode" => Self::EnterPlanMode,
            "ExitPlanMode" => Self::ExitPlanMode,
            "Bash" => Self::Bash,
            "Read" => Self::Read,
            "Write" => Self::Write,
            "Edit" => Self::Edit,
            "Grep" => Self::Grep,
            "Glob" => Self::Glob,
            "Task" => Self::Task,
            "WebSearch" => Self::WebSearch,
            "WebFetch" => Self::WebFetch,
            "TodoWrite" => Self::TodoWrite,
            "NotebookEdit" => Self::NotebookEdit,
            "NotebookRead" => Self::NotebookRead,
            _ => return None,
        })
    }
}

impl From<&str> for Tool {
    fn from(s: &str) -> Self {
        // Trim leading/trailing whitespace so " Bash " still resolves
        // to the canonical variant — preserves earlier `is_interactive_tool`
        // tolerance behavior.
        let trimmed = s.trim();
        Self::try_from_known(trimmed).unwrap_or_else(|| Self::Other(trimmed.to_string()))
    }
}

impl From<String> for Tool {
    fn from(s: String) -> Self {
        // Avoid allocation when we hit a known variant by checking the
        // borrow first; only fall back to consuming the String on Other.
        let trimmed = s.trim();
        if let Some(known) = Self::try_from_known(trimmed) {
            return known;
        }
        // Reuse the owned String when no whitespace was trimmed.
        if trimmed.len() == s.len() {
            Self::Other(s)
        } else {
            Self::Other(trimmed.to_string())
        }
    }
}

impl From<Tool> for String {
    fn from(t: Tool) -> Self {
        match t {
            Tool::Other(s) => s,
            other => other.as_str().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tools_roundtrip() {
        for variant in [
            Tool::AskUserQuestion,
            Tool::EnterPlanMode,
            Tool::ExitPlanMode,
            Tool::Bash,
            Tool::Read,
            Tool::Write,
            Tool::Edit,
            Tool::Grep,
            Tool::Glob,
            Tool::Task,
            Tool::WebSearch,
            Tool::WebFetch,
            Tool::TodoWrite,
            Tool::NotebookEdit,
            Tool::NotebookRead,
        ] {
            let s = variant.as_str().to_string();
            assert_eq!(Tool::from(s), variant);
        }
    }

    #[test]
    fn unknown_tool_round_trips_via_other() {
        assert_eq!(
            Tool::from("mcp__github__list_issues"),
            Tool::Other("mcp__github__list_issues".to_string())
        );
        assert_eq!(
            Tool::from("custom_pi_tool"),
            Tool::Other("custom_pi_tool".to_string())
        );
    }

    #[test]
    fn is_interactive_only_for_three() {
        assert!(Tool::AskUserQuestion.is_interactive());
        assert!(Tool::EnterPlanMode.is_interactive());
        assert!(Tool::ExitPlanMode.is_interactive());

        for not_interactive in [
            Tool::Bash,
            Tool::Read,
            Tool::Write,
            Tool::Edit,
            Tool::Grep,
            Tool::Glob,
            Tool::Task,
            Tool::WebSearch,
            Tool::WebFetch,
            Tool::TodoWrite,
            Tool::NotebookEdit,
            Tool::NotebookRead,
            Tool::Other("anything".into()),
        ] {
            assert!(
                !not_interactive.is_interactive(),
                "{not_interactive} should not be interactive"
            );
        }
    }

    #[test]
    fn whitespace_trimmed_into_canonical_variant() {
        assert_eq!(Tool::from("  AskUserQuestion  "), Tool::AskUserQuestion);
        assert_eq!(Tool::from("\tEnterPlanMode\n"), Tool::EnterPlanMode);
        assert!(Tool::from("  AskUserQuestion  ").is_interactive());
    }

    #[test]
    fn case_sensitive_match() {
        assert_eq!(
            Tool::from("askuserquestion"),
            Tool::Other("askuserquestion".into())
        );
        assert_eq!(
            Tool::from("ASKUSERQUESTION"),
            Tool::Other("ASKUSERQUESTION".into())
        );
        assert!(!Tool::from("askuserquestion").is_interactive());
    }

    #[test]
    fn empty_string_becomes_empty_other_and_is_not_interactive() {
        assert_eq!(Tool::from(""), Tool::Other(String::new()));
        assert!(!Tool::from("").is_interactive());
        assert!(!Tool::from("   ").is_interactive());
    }

    #[test]
    fn serde_roundtrip_known_and_other() {
        // Wire format is just the bare tool name string.
        assert_eq!(serde_json::to_string(&Tool::Bash).unwrap(), "\"Bash\"");
        assert_eq!(
            serde_json::to_string(&Tool::Other("mcp__plugin__do_thing".into())).unwrap(),
            "\"mcp__plugin__do_thing\""
        );

        assert_eq!(
            serde_json::from_str::<Tool>("\"Bash\"").unwrap(),
            Tool::Bash
        );
        assert_eq!(
            serde_json::from_str::<Tool>("\"mcp__x__y\"").unwrap(),
            Tool::Other("mcp__x__y".into())
        );
    }
}
