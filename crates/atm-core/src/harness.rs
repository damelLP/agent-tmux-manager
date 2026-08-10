//! Coding-agent *harness* identification.
//!
//! "Harness" matches the term used in the user-facing config (see bead
//! `agent-tmux-manager-bva`'s TOML `[harness.<name>]` sections) and in
//! pi's three-axis model (`harness × provider × model`). It's the
//! wrapper program a user runs — Claude Code, pi, future others —
//! distinct from the LLM provider/model it talks to.
//!
//! Distinct from `AgentType` (which currently enumerates Claude
//! subagent roles like `Explore`/`Plan`/`GeneralPurpose` —
//! misleadingly named; the harness axis lives here, not on
//! `AgentType`). `Harness` is the actual harness identity.
//!
//! Set at session-creation time by whichever adapter is feeding the
//! daemon (Claude adapter → `ClaudeCode`; pi adapter → `Pi`; pure
//! `/proc` discovery → `Unknown` until we wire bead `hfv`).

use serde::{Deserialize, Serialize};
use std::fmt;

/// Which coding-agent harness is driving this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    /// Claude Code (Anthropic).
    ClaudeCode,
    /// pi (<https://pi.dev/>) — provider-agnostic harness.
    Pi,
    /// OpenAI Codex CLI.
    Codex,
    /// Amp coding agent.
    Amp,
    /// Qwen Code CLI.
    Qwen,
    /// Gemini CLI.
    Gemini,
    /// Devin CLI (<https://devin.ai/>).
    Devin,
    /// GitHub Copilot CLI.
    Copilot,
    /// Unknown harness — discovered via process scanning before any
    /// adapter event arrived, or a future harness we haven't tagged.
    #[default]
    #[serde(other)]
    Unknown,
}

impl Harness {
    /// Short tag suitable for a TUI badge (`[claude]`, `[pi]`, `[?]`).
    #[must_use]
    pub fn short_tag(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Pi => "pi",
            Self::Codex => "codex",
            Self::Amp => "amp",
            Self::Qwen => "qwen",
            Self::Gemini => "gemini",
            Self::Devin => "devin",
            Self::Copilot => "copilot",
            Self::Unknown => "?",
        }
    }
}

impl fmt::Display for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ClaudeCode => "Claude Code",
            Self::Pi => "pi",
            Self::Codex => "Codex CLI",
            Self::Amp => "Amp",
            Self::Qwen => "Qwen Code",
            Self::Gemini => "Gemini CLI",
            Self::Devin => "Devin CLI",
            Self::Copilot => "GitHub Copilot CLI",
            Self::Unknown => "unknown",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Harness::ClaudeCode).unwrap(),
            "\"claude_code\""
        );
        assert_eq!(serde_json::to_string(&Harness::Pi).unwrap(), "\"pi\"");
        assert_eq!(serde_json::to_string(&Harness::Codex).unwrap(), "\"codex\"");
        assert_eq!(serde_json::to_string(&Harness::Amp).unwrap(), "\"amp\"");
        assert_eq!(serde_json::to_string(&Harness::Qwen).unwrap(), "\"qwen\"");
        assert_eq!(
            serde_json::to_string(&Harness::Gemini).unwrap(),
            "\"gemini\""
        );
        assert_eq!(serde_json::to_string(&Harness::Devin).unwrap(), "\"devin\"");
        assert_eq!(
            serde_json::to_string(&Harness::Copilot).unwrap(),
            "\"copilot\""
        );
        assert_eq!(
            serde_json::to_string(&Harness::Unknown).unwrap(),
            "\"unknown\""
        );
    }

    #[test]
    fn harness_default_is_unknown() {
        assert_eq!(Harness::default(), Harness::Unknown);
    }

    #[test]
    fn short_tag_matches_display_intent() {
        assert_eq!(Harness::ClaudeCode.short_tag(), "claude");
        assert_eq!(Harness::Pi.short_tag(), "pi");
        assert_eq!(Harness::Codex.short_tag(), "codex");
        assert_eq!(Harness::Amp.short_tag(), "amp");
        assert_eq!(Harness::Qwen.short_tag(), "qwen");
        assert_eq!(Harness::Gemini.short_tag(), "gemini");
        assert_eq!(Harness::Devin.short_tag(), "devin");
        assert_eq!(Harness::Copilot.short_tag(), "copilot");
        assert_eq!(Harness::Unknown.short_tag(), "?");
    }
}
