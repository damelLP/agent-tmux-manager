//! Parsing Claude Code JSON structures.
//!
//! Based on validated integration testing (Week 1).

use atm_core::{HookEventType, SessionDomain, SessionId, StatusLineData};
use serde::Deserialize;

/// Raw status line JSON structure from Claude Code.
///
/// Based on validated integration testing (Week 1).
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
    #[serde(default = "default_context_window_size")]
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

fn default_context_window_size() -> u32 {
    200_000
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
        let model = self.model.as_ref()?;
        let cost = self.cost.as_ref();
        let context = self.context_window.as_ref();
        let current = context.and_then(|c| c.current_usage.as_ref());

        Some(StatusLineData {
            session_id: self.session_id.clone(),
            model_id: model.id.clone(),
            model_display_name: model.display_name.clone(),
            cost_usd: cost.map(|c| c.total_cost_usd).unwrap_or(0.0),
            total_duration_ms: cost.map(|c| c.total_duration_ms).unwrap_or(0),
            api_duration_ms: cost.map(|c| c.total_api_duration_ms).unwrap_or(0),
            lines_added: cost.map(|c| c.total_lines_added).unwrap_or(0),
            lines_removed: cost.map(|c| c.total_lines_removed).unwrap_or(0),
            total_input_tokens: context.map(|c| c.total_input_tokens).unwrap_or(0),
            total_output_tokens: context.map(|c| c.total_output_tokens).unwrap_or(0),
            context_window_size: context.map(|c| c.context_window_size).unwrap_or(200_000),
            current_input_tokens: current.map(|c| c.input_tokens).unwrap_or(0),
            current_output_tokens: current.map(|c| c.output_tokens).unwrap_or(0),
            cache_creation_tokens: current.map(|c| c.cache_creation_input_tokens).unwrap_or(0),
            cache_read_tokens: current.map(|c| c.cache_read_input_tokens).unwrap_or(0),
            cwd: self.cwd.clone(),
            version: self.version.clone(),
        })
    }

    /// Converts to SessionDomain.
    /// Returns None if required fields (model) are missing.
    pub fn to_session_domain(&self) -> Option<SessionDomain> {
        let data = self.to_status_line_data()?;
        Some(SessionDomain::from_status_line(&data))
    }

    /// Updates an existing SessionDomain with new data.
    /// Only updates fields that are present in this status line.
    pub fn update_session(&self, session: &mut SessionDomain) {
        use atm_core::Model;

        // Update model if present (fills in Unknown for discovered/hook-created sessions)
        if let Some(model) = &self.model {
            let parsed = Model::from_id(&model.id);
            session.model = parsed;

            // For unknown models, store display name fallback
            if parsed.is_unknown() && !model.id.is_empty() {
                session.model_display_override = Some(
                    model
                        .display_name
                        .clone()
                        .unwrap_or_else(|| atm_core::derive_display_name(&model.id)),
                );
            } else {
                session.model_display_override = None;
            }
        }

        // Build StatusLineData for the update (model_id not used in update)
        let cost = self.cost.as_ref();
        let context = self.context_window.as_ref();
        let current = context.and_then(|c| c.current_usage.as_ref());

        let data = StatusLineData {
            session_id: self.session_id.clone(),
            model_id: String::new(),  // Not used in update
            model_display_name: None, // Not used in update
            cost_usd: cost.map(|c| c.total_cost_usd).unwrap_or(0.0),
            total_duration_ms: cost.map(|c| c.total_duration_ms).unwrap_or(0),
            api_duration_ms: cost.map(|c| c.total_api_duration_ms).unwrap_or(0),
            lines_added: cost.map(|c| c.total_lines_added).unwrap_or(0),
            lines_removed: cost.map(|c| c.total_lines_removed).unwrap_or(0),
            total_input_tokens: context.map(|c| c.total_input_tokens).unwrap_or(0),
            total_output_tokens: context.map(|c| c.total_output_tokens).unwrap_or(0),
            context_window_size: context.map(|c| c.context_window_size).unwrap_or(200_000),
            current_input_tokens: current.map(|c| c.input_tokens).unwrap_or(0),
            current_output_tokens: current.map(|c| c.output_tokens).unwrap_or(0),
            cache_creation_tokens: current.map(|c| c.cache_creation_input_tokens).unwrap_or(0),
            cache_read_tokens: current.map(|c| c.cache_read_input_tokens).unwrap_or(0),
            cwd: self.cwd.clone(),
            version: self.version.clone(),
        };

        session.update_from_status_line(&data);
    }
}

/// Raw hook event JSON structure from Claude Code.
///
/// Flat structure with all possible fields as Option<T>.
/// Use typed conversion for domain-layer type safety.
#[derive(Debug, Clone, Deserialize)]
pub struct RawHookEvent {
    // === Common Fields (all events) ===
    pub session_id: String,
    pub hook_event_name: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,

    // === Injected by hook script ===
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub tmux_pane: Option<String>,

    // === Tool Events (PreToolUse, PostToolUse, PostToolUseFailure) ===
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_response: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_use_id: Option<String>,

    // === User Prompt (UserPromptSubmit) ===
    #[serde(default)]
    pub prompt: Option<String>,

    // === Stop Events (Stop, SubagentStop) ===
    #[serde(default)]
    pub stop_hook_active: Option<bool>,

    // === Subagent Events (SubagentStart, SubagentStop) ===
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub agent_transcript_path: Option<String>,

    // === Session Events (SessionStart, SessionEnd) ===
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub model: Option<String>,

    // === Compaction/Setup (PreCompact, Setup) ===
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub custom_instructions: Option<String>,

    // === Notification ===
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

impl RawHookEvent {
    /// Parses the hook event type.
    pub fn event_type(&self) -> Option<HookEventType> {
        HookEventType::from_event_name(&self.hook_event_name)
    }

    /// Returns the session ID.
    pub fn session_id(&self) -> SessionId {
        SessionId::new(&self.session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::Model;

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
    fn test_raw_hook_event_parsing() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash"
        }"#;

        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(HookEventType::PreToolUse));
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));
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
    fn test_raw_hook_event_stop() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "Stop",
            "stop_hook_active": true
        }"#;

        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(HookEventType::Stop));
        assert_eq!(event.stop_hook_active, Some(true));
    }

    #[test]
    fn test_raw_hook_event_user_prompt() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "Help me write a function"
        }"#;

        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(HookEventType::UserPromptSubmit));
        assert_eq!(event.prompt.as_deref(), Some("Help me write a function"));
    }

    #[test]
    fn test_raw_hook_event_subagent_start() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "SubagentStart",
            "agent_id": "agent_456",
            "agent_type": "Explore"
        }"#;

        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(HookEventType::SubagentStart));
        assert_eq!(event.agent_id.as_deref(), Some("agent_456"));
        assert_eq!(event.agent_type.as_deref(), Some("Explore"));
    }

    #[test]
    fn test_raw_hook_event_notification() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
            "message": "Allow tool execution?"
        }"#;

        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(HookEventType::Notification));
        assert_eq!(
            event.notification_type.as_deref(),
            Some("permission_prompt")
        );
    }

    #[test]
    fn test_raw_hook_event_session_start() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "SessionStart",
            "source": "resume",
            "model": "claude-opus-4-5-20251101"
        }"#;

        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(HookEventType::SessionStart));
        assert_eq!(event.source.as_deref(), Some("resume"));
    }

    #[test]
    fn test_raw_hook_event_pre_compact() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "PreCompact",
            "trigger": "auto"
        }"#;

        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(HookEventType::PreCompact));
        assert_eq!(event.trigger.as_deref(), Some("auto"));
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
