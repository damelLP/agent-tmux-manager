//! Raw Codex CLI hook-event payload (the JSON Codex sends on stdin to
//! the hook script).
//!
//! Flat structure with all possible fields as `Option<T>` — Codex
//! emits the same envelope shape for every event and only fills in the
//! relevant fields. Use [`RawCodexEvent::to_lifecycle_event`] to
//! convert into a vendor-neutral `LifecycleEvent`.
//!
//! Field inventory validated against live codex-cli 0.146.1 traffic
//! (2026-08-10 spike): common fields are `session_id`,
//! `hook_event_name`, `cwd`, `transcript_path`, `model`,
//! `permission_mode`, plus `turn_id` on turn-scoped events. Codex does
//! **not** send a `pid` — the `atm-codex-hook` script injects `pid`
//! and `tmux_pane` before forwarding to the daemon.

use atm_core::SessionId;
use serde::Deserialize;

use crate::event::CodexEventType;

/// Raw hook event JSON structure from the Codex CLI.
///
/// Use typed conversion ([`Self::to_lifecycle_event`]) for domain-layer
/// type safety; the daemon never matches on `hook_event_name` strings.
#[derive(Debug, Clone, Deserialize)]
pub struct RawCodexEvent {
    // === Common Fields (all events) ===
    pub session_id: String,
    pub hook_event_name: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Present on turn-scoped events (tool/prompt/stop), absent on
    /// session-scoped ones (SessionStart/SessionEnd).
    #[serde(default)]
    pub turn_id: Option<String>,

    // === Injected by atm-codex-hook script ===
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub tmux_pane: Option<String>,

    // === Tool Events (PreToolUse, PostToolUse, PermissionRequest) ===
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    /// Raw tool output. Observed as a plain string in practice; kept as
    /// `Value` to tolerate future schema drift. Carries **no** error
    /// marker (spike-verified).
    #[serde(default)]
    pub tool_response: Option<serde_json::Value>,
    /// Present on PreToolUse/PostToolUse; absent on PermissionRequest
    /// (spike-verified).
    #[serde(default)]
    pub tool_use_id: Option<String>,

    // === User Prompt (UserPromptSubmit) ===
    #[serde(default)]
    pub prompt: Option<String>,

    // === Stop Events (Stop, SubagentStop) ===
    #[serde(default)]
    pub stop_hook_active: Option<bool>,
    #[serde(default)]
    pub last_assistant_message: Option<String>,

    // === Subagent Events (SubagentStart, SubagentStop) ===
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub agent_transcript_path: Option<String>,

    // === Session Events (SessionStart, SessionEnd) ===
    /// SessionStart: `startup` / `resume` / `clear` / `compact`.
    #[serde(default)]
    pub source: Option<String>,
    /// SessionEnd: observed value `other` (spike); carried verbatim.
    #[serde(default)]
    pub reason: Option<String>,

    // === Compaction (PreCompact, PostCompact) ===
    /// PreCompact: `manual` / `auto`.
    #[serde(default)]
    pub trigger: Option<String>,
}

impl RawCodexEvent {
    /// Parses the hook event type.
    pub fn event_type(&self) -> Option<CodexEventType> {
        CodexEventType::from_event_name(&self.hook_event_name)
    }

    /// Returns the session ID.
    pub fn session_id(&self) -> SessionId {
        SessionId::new(&self.session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The JSON literals below are trimmed copies of real payloads
    // captured from codex-cli 0.146.1 during the 2026-08-10 spike
    // (scratchpad codex-spike/spike-log*.jsonl).

    #[test]
    fn parse_session_start_from_spike_capture() {
        let json = r#"{
            "session_id": "019fecd6-44bf-7482-9eaa-ab1f81fff651",
            "transcript_path": "/home/user/.codex/sessions/2026/08/10/rollout.jsonl",
            "cwd": "/tmp/project",
            "hook_event_name": "SessionStart",
            "model": "gpt-5.6-sol",
            "permission_mode": "bypassPermissions",
            "source": "startup"
        }"#;
        let event: RawCodexEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(CodexEventType::SessionStart));
        assert_eq!(event.source.as_deref(), Some("startup"));
        assert_eq!(
            event.turn_id, None,
            "session-scoped events carry no turn_id"
        );
    }

    #[test]
    fn parse_user_prompt_submit_from_spike_capture() {
        let json = r#"{
            "session_id": "019fecd6-44bf-7482-9eaa-ab1f81fff651",
            "turn_id": "019fecd6-453c-7902-9dbb-64b9494a85ec",
            "hook_event_name": "UserPromptSubmit",
            "model": "gpt-5.6-sol",
            "permission_mode": "bypassPermissions",
            "prompt": "Run the shell command echo hello"
        }"#;
        let event: RawCodexEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(CodexEventType::UserPromptSubmit));
        assert_eq!(
            event.prompt.as_deref(),
            Some("Run the shell command echo hello")
        );
        assert!(event.turn_id.is_some());
    }

    #[test]
    fn parse_pre_tool_use_from_spike_capture() {
        let json = r#"{
            "session_id": "019fecd6-44bf-7482-9eaa-ab1f81fff651",
            "turn_id": "019fecd6-453c-7902-9dbb-64b9494a85ec",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hello from atm spike"},
            "tool_use_id": "exec-0faab0a6-ad36-4a79-95ab-f7b615a12706"
        }"#;
        let event: RawCodexEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(CodexEventType::PreToolUse));
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));
        assert!(event.tool_use_id.is_some());
    }

    #[test]
    fn parse_post_tool_use_string_response_from_spike_capture() {
        // tool_response is a plain string of raw output — not an
        // object, and with no error marker even for a failed command
        // (spike-verified against a genuinely failing tool call).
        let json = r#"{
            "session_id": "019fecd6-44bf-7482-9eaa-ab1f81fff651",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": "exec-0faab0a6-ad36-4a79-95ab-f7b615a12706",
            "tool_response": "thread 'main' panicked at linux-sandbox: Permission denied"
        }"#;
        let event: RawCodexEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(CodexEventType::PostToolUse));
        assert!(event.tool_response.as_ref().is_some_and(|v| v.is_string()));
    }

    #[test]
    fn parse_permission_request_from_spike_capture() {
        // PermissionRequest carries tool_name + tool_input but no
        // tool_use_id (spike-verified).
        let json = r#"{
            "session_id": "019fecd8-9dca-7fe3-b198-1e412b3dfa36",
            "turn_id": "019fecd8-f6bf-7fc0-9d73-a08d91147db1",
            "hook_event_name": "PermissionRequest",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "tool_name": "Bash",
            "tool_input": {"command": "touch spike-permission-test.txt"}
        }"#;
        let event: RawCodexEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(CodexEventType::PermissionRequest));
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));
        assert_eq!(event.tool_use_id, None);
    }

    #[test]
    fn parse_stop_from_spike_capture() {
        let json = r#"{
            "session_id": "019fecd6-44bf-7482-9eaa-ab1f81fff651",
            "hook_event_name": "Stop",
            "stop_hook_active": false,
            "last_assistant_message": "done"
        }"#;
        let event: RawCodexEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(CodexEventType::Stop));
        assert_eq!(event.stop_hook_active, Some(false));
        assert_eq!(event.last_assistant_message.as_deref(), Some("done"));
    }

    #[test]
    fn parse_session_end_from_spike_capture() {
        let json = r#"{
            "session_id": "019fecd6-44bf-7482-9eaa-ab1f81fff651",
            "hook_event_name": "SessionEnd",
            "reason": "other"
        }"#;
        let event: RawCodexEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(CodexEventType::SessionEnd));
        assert_eq!(event.reason.as_deref(), Some("other"));
    }

    #[test]
    fn parse_with_injected_pid_and_tmux_pane() {
        // The atm-codex-hook script merges pid + tmux_pane into the
        // vendor payload before forwarding, mirroring atm-hook.
        let json = r#"{
            "session_id": "s-1",
            "hook_event_name": "SessionStart",
            "pid": 567294,
            "tmux_pane": "%18"
        }"#;
        let event: RawCodexEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.pid, Some(567294));
        assert_eq!(event.tmux_pane.as_deref(), Some("%18"));
    }

    #[test]
    fn unknown_event_name_parses_but_has_no_type() {
        let json = r#"{
            "session_id": "s-1",
            "hook_event_name": "SomeFutureEvent"
        }"#;
        let event: RawCodexEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), None);
    }
}
