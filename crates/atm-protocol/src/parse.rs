//! Parsing of vendor-shaped wire payloads carried by `MessageType` envelopes.
//!
//! Currently houses Claude's status-line JSON parser. The
//! Claude raw hook event vocabulary lives in the `atm-claude-adapter`
//! crate; pi-shaped wire data lives in `atm-pi-adapter`. Future:
//! `RawStatusLine` likely also moves to `atm-claude-adapter`, with its
//! fixture.
//!
//! ## Captured payloads
//!
//! `tests/fixtures/claude_status_line_2.1.310.jsonl` holds 14 status
//! lines, one per line, as Claude wrote them to the statusLine command's
//! stdin. Captured: Claude Code 2.1.310, 2026-10-24, the TUI run
//! described under "Captured payloads" in `atm-claude-adapter`'s crate
//! docs (a local fake Messages API, no model call; paths rewritten to
//! `/home/user`). Every field `RawStatusLine` reads that Claude supplies
//! is on every line; `pid` and `tmux_pane` are not, because Claude sends
//! neither and `atm-hook` adds them before atmd sees the line.
//! `current_usage` and `used_percentage` are `null` until a session's
//! first request, including the session `/clear` starts. The lines also
//! carry fields the parser ignores: `effort`, `thinking`, `fast_mode`,
//! `output_style`, `prompt_cache`, `prompt_id` and
//! `workspace.added_dirs`.
//!
//! Claude's mapping violates atm-core's cumulative contract for
//! `ContextUsage::total_input_tokens` and `total_output_tokens`, which
//! pi and Codex fill with session totals; no consumer reads them today.
//! Claude's `context_window.total_input_tokens` is not a session total.
//! On every line it equals the sum of the latest request's
//! `input_tokens`, `cache_creation_input_tokens` and
//! `cache_read_input_tokens`, and `total_output_tokens` stays at one
//! request's 42 across requests. Both drop to 0 after `/compact`, while
//! `cost.total_cost_usd` keeps growing. The 2.1.302 capture below shows
//! the same: its 45772 is the sum of its `current_usage` input fields.
//!
//! `test_update_session_applies_context_window_size` uses one status
//! line trimmed to the fields read. Captured: Claude Code 2.1.302, a
//! `--settings` statusLine override over startup, a turn, `/clear` and
//! another turn; added in ada96c5 on 2026-10-18. The other test payloads
//! are hand-written (see `Synthetic:` there).

use atm_core::{SessionDomain, StatusLineData};
use serde::Deserialize;

/// Raw status line JSON structure from Claude Code.
///
/// Written in c2a89f0 on 2026-01-25 from "validated integration testing
/// (Week 1)", Claude Code version not recorded. Re-checked against
/// Claude Code 2.1.310 (see the module docs).
/// All fields except session_id are optional to handle partial updates.
#[derive(Debug, Clone, Deserialize)]
pub struct RawStatusLine {
    pub session_id: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub model: Option<RawModel>,
    #[serde(default)]
    pub workspace: Option<RawWorkspace>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub cost: Option<RawCost>,
    #[serde(default)]
    pub context_window: Option<RawContextWindow>,
    #[serde(default)]
    pub exceeds_200k_tokens: Option<bool>,
    /// Process ID of the Claude Code process (injected by status line script via $PPID)
    #[serde(default)]
    pub pid: Option<u32>,
    /// Tmux pane ID (injected by hook script via $TMUX_PANE)
    #[serde(default)]
    pub tmux_pane: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawModel {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawWorkspace {
    #[serde(default)]
    pub current_dir: Option<String>,
    #[serde(default)]
    pub project_dir: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawCost {
    pub total_cost_usd: f64,
    pub total_duration_ms: u64,
    #[serde(default)]
    pub total_api_duration_ms: u64,
    #[serde(default)]
    pub total_lines_added: u64,
    #[serde(default)]
    pub total_lines_removed: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawContextWindow {
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    /// 0 when omitted, meaning "not reported"
    #[serde(default)]
    pub context_window_size: u32,
    /// Pre-calculated percentage of context window used (0-100), provided by Claude Code
    #[serde(default)]
    pub used_percentage: Option<f64>,
    /// Pre-calculated percentage of context window remaining (0-100), provided by Claude Code
    #[serde(default)]
    pub remaining_percentage: Option<f64>,
    #[serde(default)]
    pub current_usage: Option<RawCurrentUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawCurrentUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl RawStatusLine {
    /// Converts raw JSON data to a StatusLineData struct.
    ///
    /// Returns None if required fields (model) are missing.
    pub fn to_status_line_data(&self) -> Option<StatusLineData> {
        self.model.as_ref()?;
        Some(self.status_line_data())
    }

    /// Converts to SessionDomain, tagged as a Claude Code session.
    /// Returns None if required fields (model) are missing.
    pub fn to_session_domain(&self) -> Option<SessionDomain> {
        let data = self.to_status_line_data()?;
        let mut session = SessionDomain::from_status_line(&data);
        session.harness = atm_core::Harness::ClaudeCode;
        Some(session)
    }

    /// Updates an existing SessionDomain with new data.
    /// Only updates fields that are present in this status line.
    /// Returns `true` if the working directory changed.
    pub fn update_session(&self, session: &mut SessionDomain) -> bool {
        // Core skips an empty model id as not reported, but this parser
        // has always reset the model on one.
        if self.model.as_ref().is_some_and(|m| m.id.is_empty()) {
            session.model = atm_core::Model::Unknown;
            session.model_display_override = None;
        }
        session.update_from_status_line(&self.status_line_data())
    }

    /// Maps every field. An absent model or window size maps to "" or 0,
    /// which core reads as not reported.
    fn status_line_data(&self) -> StatusLineData {
        let model = self.model.as_ref();
        let cost = self.cost.as_ref();
        let context = self.context_window.as_ref();
        let current = context.and_then(|c| c.current_usage.as_ref());

        StatusLineData {
            session_id: self.session_id.clone(),
            model_id: model.map(|m| m.id.clone()).unwrap_or_default(),
            model_display_name: model.and_then(|m| m.display_name.clone()),
            cost_usd: cost.map(|c| c.total_cost_usd).unwrap_or(0.0),
            total_duration_ms: cost.map(|c| c.total_duration_ms).unwrap_or(0),
            api_duration_ms: cost.map(|c| c.total_api_duration_ms).unwrap_or(0),
            lines_added: cost.map(|c| c.total_lines_added).unwrap_or(0),
            lines_removed: cost.map(|c| c.total_lines_removed).unwrap_or(0),
            total_input_tokens: context.map(|c| c.total_input_tokens).unwrap_or(0),
            total_output_tokens: context.map(|c| c.total_output_tokens).unwrap_or(0),
            context_window_size: context.map(|c| c.context_window_size).unwrap_or(0),
            current_input_tokens: current.map(|c| c.input_tokens).unwrap_or(0),
            current_output_tokens: current.map(|c| c.output_tokens).unwrap_or(0),
            cache_creation_tokens: current.map(|c| c.cache_creation_input_tokens).unwrap_or(0),
            cache_read_tokens: current.map(|c| c.cache_read_input_tokens).unwrap_or(0),
            cwd: self.cwd.clone(),
            version: self.version.clone(),
        }
    }
}

// `RawHookEvent` and the Claude→LifecycleEvent translation moved to the
// `atm-claude-adapter` crate (see `atm_claude_adapter::wire::RawHookEvent`
// and `atm_claude_adapter::translate`).

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::Model;

    // Synthetic: unless marked Captured, the payloads in this module are
    // hand-written, with placeholder ids (`test-123`) and models chosen
    // for the case (`gpt-4o`, `gemini-1.5-pro`), Claude Code version not
    // recorded. They were added in c2a89f0 (2026-01-25), d0147d7
    // (2026-02-11) and ada96c5 (2026-10-18).

    #[test]
    fn test_raw_status_line_parsing() {
        let json = r#"{
            "session_id": "test-123",
            "model": {"id": "claude-opus-4-5-20251101", "display_name": "Opus 4.5"},
            "cost": {"total_cost_usd": 0.35, "total_duration_ms": 35000},
            "context_window": {"total_input_tokens": 5000, "context_window_size": 200000}
        }"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        let session = raw.to_session_domain().expect("should create session");

        assert_eq!(session.id.as_str(), "test-123");
        assert_eq!(session.model, Model::Opus45);
        assert!((session.cost.as_usd() - 0.35).abs() < 0.001);
        assert_eq!(session.context.total_input_tokens.as_u64(), 5000);
    }

    #[test]
    fn test_raw_status_line_with_current_usage() {
        let json = r#"{
            "session_id": "test-456",
            "model": {"id": "claude-sonnet-4-20250514"},
            "cost": {"total_cost_usd": 0.10, "total_duration_ms": 10000},
            "context_window": {
                "total_input_tokens": 1000,
                "total_output_tokens": 500,
                "context_window_size": 200000,
                "current_usage": {
                    "input_tokens": 200,
                    "output_tokens": 100,
                    "cache_creation_input_tokens": 50,
                    "cache_read_input_tokens": 25
                }
            }
        }"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        let session = raw.to_session_domain().expect("should create session");

        assert_eq!(session.context.current_input_tokens.as_u64(), 200);
        assert_eq!(session.context.cache_creation_tokens.as_u64(), 50);
    }

    #[test]
    fn test_raw_status_line_context_from_current_usage() {
        // Context percentage is calculated from current_usage fields
        // context_tokens = cache_read + input + cache_creation
        let json = r#"{
            "session_id": "test-pct",
            "model": {"id": "claude-sonnet-4-20250514"},
            "context_window": {
                "total_input_tokens": 50000,
                "total_output_tokens": 10000,
                "context_window_size": 200000,
                "current_usage": {
                    "input_tokens": 1000,
                    "output_tokens": 500,
                    "cache_creation_input_tokens": 2000,
                    "cache_read_input_tokens": 40000
                }
            }
        }"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        let session = raw.to_session_domain().expect("should create session");

        // context_tokens = 40000 + 1000 + 2000 = 43000
        // percentage = 43000 / 200000 = 21.5%
        assert_eq!(session.context.context_tokens().as_u64(), 43_000);
        assert!((session.context.usage_percentage() - 21.5).abs() < 0.01);
    }

    #[test]
    fn test_raw_status_line_zero_without_current_usage() {
        // When current_usage is missing (like after /clear), context should be 0%
        let json = r#"{
            "session_id": "test-fallback",
            "model": {"id": "claude-sonnet-4-20250514"},
            "context_window": {
                "total_input_tokens": 50000,
                "total_output_tokens": 10000,
                "context_window_size": 200000
            }
        }"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        let session = raw.to_session_domain().expect("should create session");

        // No current_usage means context_tokens is 0, so 0%
        assert_eq!(session.context.context_tokens().as_u64(), 0);
        assert!((session.context.usage_percentage() - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_raw_status_line_missing_model_returns_none() {
        // Status line without model should not create a session
        let json = r#"{"session_id": "test-789"}"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        assert!(raw.to_session_domain().is_none());
    }

    #[test]
    fn test_update_session_fills_in_model() {
        use atm_core::{AgentType, SessionDomain};

        // Simulate a session created via discovery (model Unknown)
        let mut session = SessionDomain::new(
            atm_core::SessionId::new("test-discovered"),
            AgentType::GeneralPurpose,
            Model::Unknown,
        );
        assert_eq!(session.model, Model::Unknown);

        // Status line arrives with model info
        let json = r#"{
            "session_id": "test-discovered",
            "model": {"id": "claude-opus-4-5-20251101"},
            "cost": {"total_cost_usd": 0.50, "total_duration_ms": 10000}
        }"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        raw.update_session(&mut session);

        // Model should now be filled in
        assert_eq!(session.model, Model::Opus45);
        // Known model should not have a display override
        assert!(session.model_display_override.is_none());
    }

    #[test]
    fn test_update_session_unknown_model_with_display_name() {
        use atm_core::{AgentType, SessionDomain};

        let mut session = SessionDomain::new(
            atm_core::SessionId::new("test-non-anthropic"),
            AgentType::GeneralPurpose,
            Model::Unknown,
        );

        // Non-Anthropic model with display_name
        let json = r#"{
            "session_id": "test-non-anthropic",
            "model": {"id": "gpt-4o", "display_name": "GPT-4o"}
        }"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        raw.update_session(&mut session);

        assert_eq!(session.model, Model::Unknown);
        assert_eq!(session.model_display_override.as_deref(), Some("GPT-4o"));
    }

    #[test]
    fn test_update_session_unknown_model_without_display_name() {
        use atm_core::{AgentType, SessionDomain};

        let mut session = SessionDomain::new(
            atm_core::SessionId::new("test-unknown"),
            AgentType::GeneralPurpose,
            Model::Unknown,
        );

        // Unknown model without display_name - should derive from ID
        let json = r#"{
            "session_id": "test-unknown",
            "model": {"id": "gemini-1.5-pro"}
        }"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        raw.update_session(&mut session);

        assert_eq!(session.model, Model::Unknown);
        assert_eq!(
            session.model_display_override.as_deref(),
            Some("gemini-1.5-pro")
        );
    }

    #[test]
    fn test_update_session_resets_model_on_empty_id() {
        use atm_core::{AgentType, SessionDomain};

        // A known model, then an unknown one with its display override.
        for (model, display_override) in [(Model::Opus45, None), (Model::Unknown, Some("GPT-4o"))] {
            let mut session = SessionDomain::new(
                atm_core::SessionId::new("test-empty-id"),
                AgentType::GeneralPurpose,
                model,
            );
            session.model_display_override = display_override.map(str::to_string);

            let json = r#"{"session_id": "test-empty-id", "model": {"id": ""}}"#;
            let raw: RawStatusLine = serde_json::from_str(json).unwrap();
            raw.update_session(&mut session);

            assert_eq!(session.model, Model::Unknown);
            assert!(session.model_display_override.is_none());
        }
    }

    #[test]
    fn test_update_session_applies_context_window_size() {
        use atm_core::{AgentType, SessionDomain};

        // Hook-first session seeded with the Unknown model's 200K default
        let mut session = SessionDomain::new(
            atm_core::SessionId::new("5aaeafa7-b633-42c6-83fb-dcbe2f713074"),
            AgentType::GeneralPurpose,
            Model::Unknown,
        );

        // Captured: Claude Code 2.1.302, a `--settings` statusLine
        // override, trimmed to the fields read; added in ada96c5 on
        // 2026-10-18.
        let json = r#"{
            "session_id": "5aaeafa7-b633-42c6-83fb-dcbe2f713074",
            "model": {"id": "claude-opus-5-5", "display_name": "Opus 5.5"},
            "version": "2.1.302",
            "context_window": {
                "total_input_tokens": 45772,
                "total_output_tokens": 4,
                "context_window_size": 1000000,
                "current_usage": {
                    "input_tokens": 2,
                    "output_tokens": 4,
                    "cache_creation_input_tokens": 21643,
                    "cache_read_input_tokens": 24127
                },
                "used_percentage": 5,
                "remaining_percentage": 95
            }
        }"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        raw.update_session(&mut session);

        assert_eq!(session.context.context_window_size, 1_000_000);
        // 45772 / 1_000_000; Claude Code rounds this to used_percentage 5
        assert!((session.context.usage_percentage() - 4.5772).abs() < 0.001);
    }

    #[test]
    fn test_update_session_keeps_context_window_size_when_unreported() {
        use atm_core::{AgentType, SessionDomain};

        let mut session = SessionDomain::new(
            atm_core::SessionId::new("test-window"),
            AgentType::GeneralPurpose,
            Model::Unknown,
        );
        session.context.context_window_size = 1_000_000;

        // Partial update without context_window
        let partial = r#"{
            "session_id": "test-window",
            "model": {"id": "claude-opus-5-5"},
            "cost": {"total_cost_usd": 0.50, "total_duration_ms": 10000}
        }"#;
        let raw: RawStatusLine = serde_json::from_str(partial).unwrap();
        raw.update_session(&mut session);
        assert_eq!(session.context.context_window_size, 1_000_000);

        // A zero window would disable every context metric
        let zero = r#"{
            "session_id": "test-window",
            "model": {"id": "claude-opus-5-5"},
            "context_window": {"context_window_size": 0}
        }"#;
        let raw: RawStatusLine = serde_json::from_str(zero).unwrap();
        raw.update_session(&mut session);
        assert_eq!(session.context.context_window_size, 1_000_000);

        // context_window present but its size omitted
        let omitted = r#"{
            "session_id": "test-window",
            "model": {"id": "claude-opus-5-5"},
            "context_window": {"current_usage": {"cache_read_input_tokens": 300000}}
        }"#;
        let raw: RawStatusLine = serde_json::from_str(omitted).unwrap();
        raw.update_session(&mut session);
        assert_eq!(session.context.context_window_size, 1_000_000);
        assert!((session.context.usage_percentage() - 30.0).abs() < 0.001);
    }

    #[test]
    fn test_new_session_context_window_size_omitted_defaults_to_200k() {
        let json = r#"{
            "session_id": "test-omitted",
            "model": {"id": "claude-opus-5-5"},
            "context_window": {"current_usage": {"cache_read_input_tokens": 1000}}
        }"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        let session = raw.to_session_domain().expect("model is present");

        assert_eq!(session.context.context_window_size, 200_000);
    }

    #[test]
    fn test_new_session_opus46() {
        let json = r#"{
            "session_id": "test-opus46",
            "model": {"id": "claude-opus-4-6"}
        }"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        let session = raw.to_session_domain().expect("should create session");

        assert_eq!(session.model, Model::Opus46);
        assert!(session.model_display_override.is_none());
    }

    #[test]
    fn test_new_session_non_anthropic_model() {
        let json = r#"{
            "session_id": "test-gpt",
            "model": {"id": "gpt-4o", "display_name": "GPT-4o"}
        }"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        let session = raw.to_session_domain().expect("should create session");

        assert_eq!(session.model, Model::Unknown);
        assert_eq!(session.model_display_override.as_deref(), Some("GPT-4o"));
    }

    #[test]
    fn captured_status_lines_2_1_310_parse() {
        // Captured: Claude Code 2.1.310, 2026-10-24 (see the module docs).
        let lines: Vec<RawStatusLine> =
            include_str!("../tests/fixtures/claude_status_line_2.1.310.jsonl")
                .lines()
                .map(|line| serde_json::from_str(line).expect("fixture line"))
                .collect();
        assert_eq!(lines.len(), 14);
        let mut sessions = Vec::new();
        for raw in &lines {
            let model = raw.model.as_ref().expect("model is present");
            assert_eq!(model.id, "claude-opus-5-5");
            assert_eq!(model.display_name.as_deref(), Some("Opus 5.5"));
            let session = raw.to_session_domain().expect("model is present");
            assert_eq!(session.context.context_window_size, 1_000_000);
            assert_eq!(session.claude_code_version.as_deref(), Some("2.1.310"));
            sessions.push(session);
        }

        // Before the first request, `current_usage` is null.
        assert_eq!(sessions[0].context.context_tokens().as_u64(), 0);
        // After the first prompt's tool request, the fake provider's
        // second POST (`input_tokens` = 1000 + 2): 1002 input + 300 cache
        // creation + 4000 cache read. Its cost also covers the first POST,
        // a side request with no tools.
        assert_eq!(sessions[2].context.context_tokens().as_u64(), 5302);
        assert!((sessions[2].cost.as_usd() - 0.014292).abs() < 1e-9);
    }

    #[test]
    fn test_raw_status_line_partial_data() {
        // Status line with model but no cost/context should create session with defaults
        let json = r#"{
            "session_id": "test-partial",
            "model": {"id": "claude-sonnet-4-20250514"}
        }"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        let session = raw
            .to_session_domain()
            .expect("should create with defaults");

        assert_eq!(session.id.as_str(), "test-partial");
        assert!((session.cost.as_usd() - 0.0).abs() < 0.001);
        assert_eq!(session.context.total_input_tokens.as_u64(), 0);
    }
}
