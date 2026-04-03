//! Session domain entities and value objects.

use crate::hook::is_interactive_tool;
use crate::{AgentType, ContextUsage, HookEventType, Model, Money, TokenCount};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use tracing::debug;

// ============================================================================
// Type-Safe Identifiers
// ============================================================================

/// Unique identifier for a Claude Code session.
///
/// Wraps a UUID string (e.g., "8e11bfb5-7dc2-432b-9206-928fa5c35731").
/// Obtained from Claude Code's status line JSON `session_id` field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

/// Prefix used for pending session IDs (sessions discovered before their real ID is known).
pub const PENDING_SESSION_PREFIX: &str = "pending-";

impl SessionId {
    /// Creates a new SessionId from a string.
    ///
    /// Note: This does not validate UUID format. Claude Code provides
    /// the session_id, so we trust its format.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Creates a pending session ID from a process ID.
    ///
    /// Used when a Claude process is discovered but no transcript exists yet
    /// (e.g., session just started, no conversation has occurred).
    /// The pending session will be upgraded to the real session ID when
    /// it arrives via hook event or status line.
    pub fn pending_from_pid(pid: u32) -> Self {
        Self(format!("{PENDING_SESSION_PREFIX}{pid}"))
    }

    /// Checks if this is a pending session ID (not yet associated with real session).
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.0.starts_with(PENDING_SESSION_PREFIX)
    }

    /// Extracts the PID from a pending session ID.
    ///
    /// Returns `None` if this is not a pending session ID or the PID cannot be parsed.
    pub fn pending_pid(&self) -> Option<u32> {
        if !self.is_pending() {
            return None;
        }
        self.0
            .strip_prefix(PENDING_SESSION_PREFIX)
            .and_then(|s| s.parse().ok())
    }

    /// Returns the underlying string reference.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns a shortened display form (first 8 characters).
    ///
    /// Useful for compact TUI display.
    #[must_use]
    pub fn short(&self) -> &str {
        self.0.get(..8).unwrap_or(&self.0)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Unique identifier for a tool invocation.
///
/// Format: "toolu_..." (e.g., "toolu_01ABC123XYZ")
/// Provided by Claude Code in hook events.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolUseId(String);

impl ToolUseId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolUseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for ToolUseId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Path to a session's transcript JSONL file.
///
/// Example: "/home/user/.claude/projects/.../session.jsonl"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TranscriptPath(PathBuf);

impl TranscriptPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Returns the filename portion of the path.
    pub fn filename(&self) -> Option<&str> {
        self.0.file_name().and_then(|n| n.to_str())
    }
}

impl fmt::Display for TranscriptPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl AsRef<Path> for TranscriptPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

// ============================================================================
// Session Status (3-State Model)
// ============================================================================

/// Current operational status of a session.
///
/// Three fundamental states based on user action requirements:
/// - **Idle**: Nothing happening - Claude finished, waiting for user
/// - **Working**: Claude is actively processing - user just waits
/// - **AttentionNeeded**: User must act for session to proceed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Session is idle - Claude finished, waiting for user's next action.
    /// User can take their time, no urgency.
    #[default]
    Idle,

    /// Claude is actively processing - user just waits.
    /// Work is happening, no user action needed.
    Working,

    /// User must take action for the session to proceed.
    /// Something is blocked waiting for user input.
    AttentionNeeded,
}

impl SessionStatus {
    /// Returns the display label for this status.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::AttentionNeeded => "needs input",
        }
    }

    /// Returns the ASCII icon for this status.
    #[must_use]
    pub fn icon(&self) -> &'static str {
        match self {
            Self::Idle => "-",
            Self::Working => ">",
            Self::AttentionNeeded => "!",
        }
    }

    /// Returns true if this status should blink in the UI.
    #[must_use]
    pub fn should_blink(&self) -> bool {
        matches!(self, Self::AttentionNeeded)
    }

    /// Returns true if the session is actively processing.
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Working)
    }

    /// Returns true if user action is needed.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        matches!(self, Self::AttentionNeeded)
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => write!(f, "Idle"),
            Self::Working => write!(f, "Working"),
            Self::AttentionNeeded => write!(f, "Needs Input"),
        }
    }
}

// ============================================================================
// Activity Detail
// ============================================================================

/// Detailed information about current session activity.
///
/// Provides structured details alongside the simple SessionStatus enum.
/// This separates "what state are we in" from "what specifically is happening".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityDetail {
    /// Tool name if running/waiting on a tool
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// When the current activity started
    pub started_at: DateTime<Utc>,
    /// Additional context (e.g., "Compacting", "Setup", "Thinking")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

impl ActivityDetail {
    /// Creates a new ActivityDetail for a tool operation.
    pub fn new(tool_name: &str) -> Self {
        Self {
            tool_name: Some(tool_name.to_string()),
            started_at: Utc::now(),
            context: None,
        }
    }

    /// Creates an ActivityDetail with context but no specific tool.
    pub fn with_context(context: &str) -> Self {
        Self {
            tool_name: None,
            started_at: Utc::now(),
            context: Some(context.to_string()),
        }
    }

    /// Creates an ActivityDetail for "thinking" state.
    pub fn thinking() -> Self {
        Self::with_context("Thinking")
    }

    /// Returns how long this activity has been running.
    pub fn duration(&self) -> chrono::Duration {
        Utc::now().signed_duration_since(self.started_at)
    }

    /// Returns a display string for this activity.
    ///
    /// Returns a `Cow<str>` for zero-copy access when possible.
    #[must_use]
    pub fn display(&self) -> Cow<'_, str> {
        if let Some(ref tool) = self.tool_name {
            Cow::Borrowed(tool)
        } else if let Some(ref ctx) = self.context {
            Cow::Borrowed(ctx)
        } else {
            Cow::Borrowed("Unknown")
        }
    }
}

impl Default for ActivityDetail {
    fn default() -> Self {
        Self::thinking()
    }
}

// ============================================================================
// Value Objects
// ============================================================================

/// Duration tracking for a session.
///
/// Based on Claude Code status line `cost.total_duration_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionDuration {
    /// Total duration in milliseconds
    total_ms: u64,
    /// API call duration in milliseconds (time spent waiting for Claude)
    api_ms: u64,
}

impl SessionDuration {
    /// Creates a new SessionDuration.
    pub fn new(total_ms: u64, api_ms: u64) -> Self {
        Self { total_ms, api_ms }
    }

    /// Creates from total duration only.
    pub fn from_total_ms(total_ms: u64) -> Self {
        Self {
            total_ms,
            api_ms: 0,
        }
    }

    /// Returns total duration in milliseconds.
    pub fn total_ms(&self) -> u64 {
        self.total_ms
    }

    /// Returns API duration in milliseconds.
    pub fn api_ms(&self) -> u64 {
        self.api_ms
    }

    /// Returns total duration as seconds (float).
    pub fn total_seconds(&self) -> f64 {
        self.total_ms as f64 / 1000.0
    }

    /// Returns the overhead time (total - API).
    pub fn overhead_ms(&self) -> u64 {
        self.total_ms.saturating_sub(self.api_ms)
    }

    /// Formats duration for display.
    ///
    /// Returns format like "35s", "2m 15s", "1h 30m"
    pub fn format(&self) -> String {
        let secs = self.total_ms / 1000;
        if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            let mins = secs / 60;
            let remaining_secs = secs % 60;
            if remaining_secs == 0 {
                format!("{mins}m")
            } else {
                format!("{mins}m {remaining_secs}s")
            }
        } else {
            let hours = secs / 3600;
            let remaining_mins = (secs % 3600) / 60;
            if remaining_mins == 0 {
                format!("{hours}h")
            } else {
                format!("{hours}h {remaining_mins}m")
            }
        }
    }

    /// Formats duration compactly.
    pub fn format_compact(&self) -> String {
        let secs = self.total_ms / 1000;
        if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            let mins = secs / 60;
            format!("{mins}m")
        } else {
            let hours = secs / 3600;
            format!("{hours}h")
        }
    }
}

impl fmt::Display for SessionDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format())
    }
}

/// Tracks lines added and removed in a session.
///
/// Based on Claude Code status line `cost.total_lines_added/removed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LinesChanged {
    /// Lines added
    pub added: u64,
    /// Lines removed
    pub removed: u64,
}

impl LinesChanged {
    /// Creates new LinesChanged.
    pub fn new(added: u64, removed: u64) -> Self {
        Self { added, removed }
    }

    /// Returns net change (added - removed).
    pub fn net(&self) -> i64 {
        self.added as i64 - self.removed as i64
    }

    /// Returns total churn (added + removed).
    pub fn churn(&self) -> u64 {
        self.added.saturating_add(self.removed)
    }

    /// Returns true if no changes have been made.
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }

    /// Formats for display (e.g., "+150 -30").
    pub fn format(&self) -> String {
        format!("+{} -{}", self.added, self.removed)
    }

    /// Formats net change with sign.
    pub fn format_net(&self) -> String {
        let net = self.net();
        if net >= 0 {
            format!("+{net}")
        } else {
            format!("{net}")
        }
    }
}

impl fmt::Display for LinesChanged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format())
    }
}

// ============================================================================
// Status Line Data Transfer Object
// ============================================================================

/// Data extracted from Claude Code's status line JSON.
///
/// This struct consolidates the many parameters previously passed to
/// `SessionDomain::from_status_line()` and `update_from_status_line()`,
/// providing named fields for clarity and reducing error-prone parameter ordering.
#[derive(Debug, Clone, Default)]
pub struct StatusLineData {
    /// Session ID from Claude Code
    pub session_id: String,
    /// Model ID (e.g., "claude-sonnet-4-20250514")
    pub model_id: String,
    /// Display name from the provider (e.g., "Claude Opus 4.6"), if provided
    pub model_display_name: Option<String>,
    /// Total cost in USD
    pub cost_usd: f64,
    /// Total session duration in milliseconds
    pub total_duration_ms: u64,
    /// Time spent waiting for API responses in milliseconds
    pub api_duration_ms: u64,
    /// Lines of code added
    pub lines_added: u64,
    /// Lines of code removed
    pub lines_removed: u64,
    /// Total input tokens across all requests
    pub total_input_tokens: u64,
    /// Total output tokens across all responses
    pub total_output_tokens: u64,
    /// Context window size for the model
    pub context_window_size: u32,
    /// Input tokens in current context
    pub current_input_tokens: u64,
    /// Output tokens in current context
    pub current_output_tokens: u64,
    /// Tokens used for cache creation
    pub cache_creation_tokens: u64,
    /// Tokens read from cache
    pub cache_read_tokens: u64,
    /// Current working directory
    pub cwd: Option<String>,
    /// Claude Code version
    pub version: Option<String>,
}

// ============================================================================
// Domain Entity
// ============================================================================

/// Core domain model for a Claude Code session.
///
/// Contains pure business logic and state. Does NOT include
/// infrastructure concerns (PIDs, sockets, file paths).
///
/// Consistent with CONCURRENCY_MODEL.md RegistryActor ownership.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDomain {
    /// Unique session identifier
    pub id: SessionId,

    /// Type of agent (main, subagent, etc.)
    pub agent_type: AgentType,

    /// Claude model being used
    pub model: Model,

    /// Display name override for unknown/non-Anthropic models.
    /// When `model` is `Unknown`, this holds the raw model ID or
    /// the provider-supplied display name for UI rendering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_display_override: Option<String>,

    /// Current session status (3-state model)
    pub status: SessionStatus,

    /// Current activity details (tool name, context, timing)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_activity: Option<ActivityDetail>,

    /// Context window usage
    pub context: ContextUsage,

    /// Accumulated cost
    pub cost: Money,

    /// Session duration tracking
    pub duration: SessionDuration,

    /// Lines of code changed
    pub lines_changed: LinesChanged,

    /// When the session started
    pub started_at: DateTime<Utc>,

    /// Last activity timestamp
    pub last_activity: DateTime<Utc>,

    /// Working directory (project root)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,

    /// Claude Code version
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_code_version: Option<String>,

    /// Tmux pane ID (e.g., "%5") if session is running in tmux
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux_pane: Option<String>,

    /// Git project root (resolved from working_directory).
    /// Shared across all worktrees of the same repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,

    /// Git worktree path (specific checkout directory).
    /// For the main checkout, this equals project_root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,

    /// Git branch name for this worktree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,

    /// Parent session ID (set when this session is a subagent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,

    /// Child subagent session IDs spawned by this session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_session_ids: Vec<SessionId>,
}

impl SessionDomain {
    /// Creates a new SessionDomain with required fields.
    pub fn new(id: SessionId, agent_type: AgentType, model: Model) -> Self {
        let now = Utc::now();
        Self {
            id,
            agent_type,
            model,
            model_display_override: None,
            status: SessionStatus::Idle,
            current_activity: None,
            context: ContextUsage::new(model.context_window_size()),
            cost: Money::zero(),
            duration: SessionDuration::default(),
            lines_changed: LinesChanged::default(),
            started_at: now,
            last_activity: now,
            working_directory: None,
            claude_code_version: None,
            tmux_pane: None,
            project_root: None,
            worktree_path: None,
            worktree_branch: None,
            parent_session_id: None,
            child_session_ids: Vec::new(),
        }
    }

    /// Creates a SessionDomain from Claude Code status line data.
    pub fn from_status_line(data: &StatusLineData) -> Self {
        use crate::model::derive_display_name;

        let model = Model::from_id(&data.model_id);

        let mut session = Self::new(
            SessionId::new(&data.session_id),
            AgentType::GeneralPurpose, // Default, may be updated by hook events
            model,
        );

        // For unknown models, store a display name fallback:
        // prefer provider-supplied display_name, then derive from raw ID
        if model.is_unknown() && !data.model_id.is_empty() {
            session.model_display_override = Some(
                data.model_display_name
                    .clone()
                    .unwrap_or_else(|| derive_display_name(&data.model_id)),
            );
        }

        session.cost = Money::from_usd(data.cost_usd);
        session.duration = SessionDuration::new(data.total_duration_ms, data.api_duration_ms);
        session.lines_changed = LinesChanged::new(data.lines_added, data.lines_removed);
        session.context = ContextUsage {
            total_input_tokens: TokenCount::new(data.total_input_tokens),
            total_output_tokens: TokenCount::new(data.total_output_tokens),
            context_window_size: data.context_window_size,
            current_input_tokens: TokenCount::new(data.current_input_tokens),
            current_output_tokens: TokenCount::new(data.current_output_tokens),
            cache_creation_tokens: TokenCount::new(data.cache_creation_tokens),
            cache_read_tokens: TokenCount::new(data.cache_read_tokens),
        };
        session.working_directory = data.cwd.clone();
        session.claude_code_version = data.version.clone();
        session.last_activity = Utc::now();

        session
    }

    /// Updates the session with new status line data.
    ///
    /// When `current_usage` is null in Claude's status line, all current_* values
    /// will be 0, which correctly resets context percentage to 0%.
    pub fn update_from_status_line(&mut self, data: &StatusLineData) {
        self.cost = Money::from_usd(data.cost_usd);
        self.duration = SessionDuration::new(data.total_duration_ms, data.api_duration_ms);
        self.lines_changed = LinesChanged::new(data.lines_added, data.lines_removed);
        self.context.total_input_tokens = TokenCount::new(data.total_input_tokens);
        self.context.total_output_tokens = TokenCount::new(data.total_output_tokens);
        self.context.current_input_tokens = TokenCount::new(data.current_input_tokens);
        self.context.current_output_tokens = TokenCount::new(data.current_output_tokens);
        self.context.cache_creation_tokens = TokenCount::new(data.cache_creation_tokens);
        self.context.cache_read_tokens = TokenCount::new(data.cache_read_tokens);
        self.last_activity = Utc::now();

        // Status line update means Claude is working
        // Don't override AttentionNeeded (permission wait)
        if self.status != SessionStatus::AttentionNeeded {
            self.status = SessionStatus::Working;
        }
    }

    /// Updates status based on a hook event.
    pub fn apply_hook_event(&mut self, event_type: HookEventType, tool_name: Option<&str>) {
        self.last_activity = Utc::now();

        match event_type {
            HookEventType::PreToolUse => {
                if let Some(name) = tool_name {
                    if is_interactive_tool(name) {
                        self.status = SessionStatus::AttentionNeeded;
                        self.current_activity = Some(ActivityDetail::new(name));
                    } else {
                        self.status = SessionStatus::Working;
                        self.current_activity = Some(ActivityDetail::new(name));
                    }
                }
            }
            HookEventType::PostToolUse | HookEventType::PostToolUseFailure => {
                self.status = SessionStatus::Working;
                self.current_activity = Some(ActivityDetail::thinking());
            }
            HookEventType::UserPromptSubmit => {
                self.status = SessionStatus::Working;
                self.current_activity = None;
            }
            HookEventType::Stop => {
                self.status = SessionStatus::Idle;
                self.current_activity = None;
            }
            HookEventType::SessionStart => {
                self.status = SessionStatus::Idle;
                self.current_activity = None;
            }
            HookEventType::SessionEnd => {
                // Session will be removed by registry
                self.status = SessionStatus::Idle;
                self.current_activity = None;
            }
            HookEventType::PreCompact => {
                self.status = SessionStatus::Working;
                self.current_activity = Some(ActivityDetail::with_context("Compacting"));
            }
            HookEventType::Setup => {
                self.status = SessionStatus::Working;
                self.current_activity = Some(ActivityDetail::with_context("Setup"));
            }
            HookEventType::Notification => {
                // Notification handling is done separately with notification_type
                // This is a fallback - don't change status
            }
            HookEventType::SubagentStart | HookEventType::SubagentStop => {
                // Subagent tracking deferred to future PR
                self.status = SessionStatus::Working;
            }
        }
    }

    /// Updates status based on a notification event.
    pub fn apply_notification(&mut self, notification_type: Option<&str>) {
        self.last_activity = Utc::now();

        match notification_type {
            Some("permission_prompt") => {
                self.status = SessionStatus::AttentionNeeded;
                self.current_activity = Some(ActivityDetail::with_context("Permission"));
            }
            Some("idle_prompt") => {
                self.status = SessionStatus::Idle;
                self.current_activity = None;
            }
            Some("elicitation_dialog") => {
                self.status = SessionStatus::AttentionNeeded;
                self.current_activity = Some(ActivityDetail::with_context("MCP Input"));
            }
            _ => {
                // Informational notification - no status change
            }
        }
    }

    /// Returns the session age (time since started).
    pub fn age(&self) -> chrono::Duration {
        Utc::now().signed_duration_since(self.started_at)
    }

    /// Returns time since last activity.
    pub fn time_since_activity(&self) -> chrono::Duration {
        Utc::now().signed_duration_since(self.last_activity)
    }

    /// Returns true if context usage needs attention.
    pub fn needs_context_attention(&self) -> bool {
        self.context.is_warning() || self.context.is_critical()
    }
}

impl Default for SessionDomain {
    fn default() -> Self {
        Self::new(
            SessionId::new("unknown"),
            AgentType::default(),
            Model::default(),
        )
    }
}

// ============================================================================
// Infrastructure Entity
// ============================================================================

/// Record of a tool invocation.
#[derive(Debug, Clone)]
pub struct ToolUsageRecord {
    /// Name of the tool (e.g., "Bash", "Read", "Write")
    pub tool_name: String,
    /// Unique ID for this tool invocation
    pub tool_use_id: Option<ToolUseId>,
    /// When the tool was invoked
    pub timestamp: DateTime<Utc>,
}

/// Infrastructure-level data for a session.
///
/// Contains OS/system concerns that don't belong in the domain model.
/// Owned by RegistryActor alongside SessionDomain.
#[derive(Debug, Clone)]
pub struct SessionInfrastructure {
    /// Process ID of the Claude Code process (if known)
    pub pid: Option<u32>,

    /// Process start time in clock ticks (from /proc/{pid}/stat field 22).
    /// Used to detect PID reuse - if the start time changes, it's a different process.
    pub process_start_time: Option<u64>,

    /// Path to the Unix socket for this session (if applicable)
    pub socket_path: Option<PathBuf>,

    /// Path to the transcript JSONL file
    pub transcript_path: Option<TranscriptPath>,

    /// Recent tool usage history (bounded FIFO queue)
    pub recent_tools: VecDeque<ToolUsageRecord>,

    /// Number of status updates received
    pub update_count: u64,

    /// Number of hook events received
    pub hook_event_count: u64,

    /// Last error encountered (for debugging)
    pub last_error: Option<String>,
}

impl SessionInfrastructure {
    /// Maximum number of tool records to keep.
    const MAX_TOOL_HISTORY: usize = 50;

    /// Creates new SessionInfrastructure.
    pub fn new() -> Self {
        Self {
            pid: None,
            process_start_time: None,
            socket_path: None,
            transcript_path: None,
            recent_tools: VecDeque::with_capacity(Self::MAX_TOOL_HISTORY),
            update_count: 0,
            hook_event_count: 0,
            last_error: None,
        }
    }

    /// Sets the process ID and captures the process start time for PID reuse detection.
    ///
    /// The start time is read from `/proc/{pid}/stat` field 22 (starttime in clock ticks).
    /// If the PID is already set with the same value, this is a no-op.
    ///
    /// # Validation
    ///
    /// The PID is only stored if:
    /// - It's non-zero (PID 0 is invalid)
    /// - We can successfully read its start time from `/proc/{pid}/stat`
    ///
    /// This prevents storing invalid PIDs that would cause incorrect liveness checks.
    pub fn set_pid(&mut self, pid: u32) {
        // PID 0 is invalid
        if pid == 0 {
            return;
        }

        // Only update if PID changed or wasn't set
        if self.pid == Some(pid) {
            return;
        }

        // Only store PID if we can read and validate its start time
        // This ensures the PID is valid and gives us PID reuse protection
        if let Some(start_time) = read_process_start_time(pid) {
            self.pid = Some(pid);
            self.process_start_time = Some(start_time);
        } else {
            debug!(
                pid = pid,
                "PID validation failed - process may have exited or is inaccessible"
            );
        }
    }

    /// Checks if the tracked process is still alive.
    ///
    /// Returns `true` if:
    /// - No PID is tracked (can't determine liveness)
    /// - The process exists and has the same start time
    ///
    /// Returns `false` if:
    /// - The process no longer exists
    /// - The PID has been reused by a different process (start time mismatch)
    pub fn is_process_alive(&self) -> bool {
        let Some(pid) = self.pid else {
            // No PID tracked - assume alive (can't determine)
            debug!(pid = ?self.pid, "is_process_alive: no PID tracked, assuming alive");
            return true;
        };

        let Some(expected_start_time) = self.process_start_time else {
            // No start time recorded - just check if process exists via procfs
            let exists = procfs::process::Process::new(pid as i32).is_ok();
            debug!(
                pid,
                exists, "is_process_alive: no start_time, checking procfs only"
            );
            return exists;
        };

        // Check if process exists and has same start time
        match read_process_start_time(pid) {
            Some(current_start_time) => {
                let alive = current_start_time == expected_start_time;
                if !alive {
                    debug!(
                        pid,
                        expected_start_time,
                        current_start_time,
                        "is_process_alive: start time MISMATCH - PID reused?"
                    );
                }
                alive
            }
            None => {
                debug!(
                    pid,
                    expected_start_time, "is_process_alive: process NOT FOUND in /proc"
                );
                false
            }
        }
    }

    /// Records a tool usage.
    pub fn record_tool_use(&mut self, tool_name: &str, tool_use_id: Option<ToolUseId>) {
        let record = ToolUsageRecord {
            tool_name: tool_name.to_string(),
            tool_use_id,
            timestamp: Utc::now(),
        };

        self.recent_tools.push_back(record);

        // Maintain bounded size using safe VecDeque operations
        while self.recent_tools.len() > Self::MAX_TOOL_HISTORY {
            self.recent_tools.pop_front();
        }

        self.hook_event_count += 1;
    }

    /// Increments the update count.
    pub fn record_update(&mut self) {
        self.update_count += 1;
    }

    /// Records an error.
    pub fn record_error(&mut self, error: &str) {
        self.last_error = Some(error.to_string());
    }

    /// Returns the most recent tool used.
    pub fn last_tool(&self) -> Option<&ToolUsageRecord> {
        self.recent_tools.back()
    }

    /// Returns recent tools (most recent first).
    pub fn recent_tools_iter(&self) -> impl Iterator<Item = &ToolUsageRecord> {
        self.recent_tools.iter().rev()
    }
}

/// Reads the process start time using the procfs crate.
///
/// The start time (in clock ticks since boot) is stable for the lifetime
/// of a process and unique enough to detect PID reuse.
///
/// Returns `None` if the process doesn't exist or can't be read.
fn read_process_start_time(pid: u32) -> Option<u64> {
    let process = procfs::process::Process::new(pid as i32).ok()?;
    let stat = process.stat().ok()?;
    Some(stat.starttime)
}

impl Default for SessionInfrastructure {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Application Layer DTO
// ============================================================================

/// Read-only view of a session for TUI display.
///
/// Immutable snapshot created from SessionDomain.
/// Implements Clone for easy distribution to multiple UI components.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionView {
    /// Session identifier
    pub id: SessionId,

    /// Short ID for display (first 8 chars)
    pub id_short: String,

    /// Agent type label
    pub agent_type: String,

    /// Model display name
    pub model: String,

    /// Current status (3-state model)
    pub status: SessionStatus,

    /// Status label for display
    pub status_label: String,

    /// Activity detail (tool name or context)
    pub activity_detail: Option<String>,

    /// Whether this status should blink
    pub should_blink: bool,

    /// Status icon
    pub status_icon: String,

    /// Context usage percentage
    pub context_percentage: f64,

    /// Context usage formatted string
    pub context_display: String,

    /// Whether context is in warning state
    pub context_warning: bool,

    /// Whether context is in critical state
    pub context_critical: bool,

    /// Cost formatted string
    pub cost_display: String,

    /// Cost in USD (for sorting)
    pub cost_usd: f64,

    /// Duration formatted string
    pub duration_display: String,

    /// Duration in seconds (for sorting)
    pub duration_seconds: f64,

    /// Lines changed formatted string
    pub lines_display: String,

    /// Working directory (shortened for display)
    pub working_directory: Option<String>,

    /// Whether session needs attention (permission wait, high context)
    pub needs_attention: bool,

    /// Time since last activity (formatted)
    pub last_activity_display: String,

    /// Session age (formatted)
    pub age_display: String,

    /// Session start time (ISO 8601)
    pub started_at: String,

    /// Last activity time (ISO 8601)
    pub last_activity: String,

    /// Tmux pane ID (e.g., "%5") if session is running in tmux
    pub tmux_pane: Option<String>,

    /// Git project root (for grouping in tree view)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,

    /// Git worktree path
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,

    /// Git branch name for this worktree
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,

    /// Parent session ID (if this is a subagent)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,

    /// Child subagent session IDs
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_session_ids: Vec<SessionId>,
}

impl SessionView {
    /// Creates a SessionView from a SessionDomain.
    pub fn from_domain(session: &SessionDomain) -> Self {
        let now = Utc::now();
        let since_activity = now.signed_duration_since(session.last_activity);
        let age = now.signed_duration_since(session.started_at);

        Self {
            id: session.id.clone(),
            id_short: session.id.short().to_string(),
            agent_type: session.agent_type.short_name().to_string(),
            model: if session.model.is_unknown() {
                session
                    .model_display_override
                    .clone()
                    .unwrap_or_else(|| session.model.display_name().to_string())
            } else {
                session.model.display_name().to_string()
            },
            status: session.status,
            status_label: session.status.label().to_string(),
            activity_detail: session
                .current_activity
                .as_ref()
                .map(|a| a.display().into_owned()),
            should_blink: session.status.should_blink(),
            status_icon: session.status.icon().to_string(),
            context_percentage: session.context.usage_percentage(),
            context_display: session.context.format(),
            context_warning: session.context.is_warning(),
            context_critical: session.context.is_critical(),
            cost_display: session.cost.format(),
            cost_usd: session.cost.as_usd(),
            duration_display: session.duration.format(),
            duration_seconds: session.duration.total_seconds(),
            lines_display: session.lines_changed.format(),
            working_directory: session.working_directory.clone().map(|p| {
                // Shorten path for display
                if p.len() > 30 {
                    format!("...{}", &p[p.len().saturating_sub(27)..])
                } else {
                    p
                }
            }),
            needs_attention: session.status.needs_attention() || session.needs_context_attention(),
            last_activity_display: format_duration(since_activity),
            age_display: format_duration(age),
            started_at: session.started_at.to_rfc3339(),
            last_activity: session.last_activity.to_rfc3339(),
            tmux_pane: session.tmux_pane.clone(),
            project_root: session.project_root.clone(),
            worktree_path: session.worktree_path.clone(),
            worktree_branch: session.worktree_branch.clone(),
            parent_session_id: session.parent_session_id.clone(),
            child_session_ids: session.child_session_ids.clone(),
        }
    }
}

impl From<&SessionDomain> for SessionView {
    fn from(session: &SessionDomain) -> Self {
        Self::from_domain(session)
    }
}

/// Formats a duration for human-readable display.
fn format_duration(duration: chrono::Duration) -> String {
    let secs = duration.num_seconds();
    if secs < 0 {
        return "now".to_string();
    }
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        let mins = secs / 60;
        format!("{mins}m ago")
    } else if secs < 86400 {
        let hours = secs / 3600;
        format!("{hours}h ago")
    } else {
        let days = secs / 86400;
        format!("{days}d ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a test session with default values.
    fn create_test_session(id: &str) -> SessionDomain {
        SessionDomain::new(SessionId::new(id), AgentType::GeneralPurpose, Model::Opus45)
    }

    #[test]
    fn test_session_id_short() {
        let id = SessionId::new("8e11bfb5-7dc2-432b-9206-928fa5c35731");
        assert_eq!(id.short(), "8e11bfb5");
    }

    #[test]
    fn test_session_id_short_short_id() {
        let id = SessionId::new("abc");
        assert_eq!(id.short(), "abc");
    }

    #[test]
    fn test_session_status_display() {
        let status = SessionStatus::Working;
        assert_eq!(format!("{status}"), "Working");
    }

    #[test]
    fn test_session_domain_creation() {
        let session = SessionDomain::new(
            SessionId::new("test-123"),
            AgentType::GeneralPurpose,
            Model::Opus45,
        );
        assert_eq!(session.id.as_str(), "test-123");
        assert_eq!(session.model, Model::Opus45);
        assert!(session.cost.is_zero());
    }

    #[test]
    fn test_session_view_from_domain() {
        let session = SessionDomain::new(
            SessionId::new("8e11bfb5-7dc2-432b-9206-928fa5c35731"),
            AgentType::Explore,
            Model::Sonnet4,
        );
        let view = SessionView::from_domain(&session);

        assert_eq!(view.id_short, "8e11bfb5");
        assert_eq!(view.agent_type, "explore");
        assert_eq!(view.model, "Sonnet 4");
    }

    #[test]
    fn test_session_view_unknown_model_with_override() {
        let mut session = SessionDomain::new(
            SessionId::new("test-override"),
            AgentType::GeneralPurpose,
            Model::Unknown,
        );
        session.model_display_override = Some("GPT-4o".to_string());

        let view = SessionView::from_domain(&session);
        assert_eq!(view.model, "GPT-4o");
    }

    #[test]
    fn test_session_view_unknown_model_without_override() {
        let session = SessionDomain::new(
            SessionId::new("test-no-override"),
            AgentType::GeneralPurpose,
            Model::Unknown,
        );

        let view = SessionView::from_domain(&session);
        assert_eq!(view.model, "Unknown");
    }

    #[test]
    fn test_session_view_known_model_ignores_override() {
        let mut session = SessionDomain::new(
            SessionId::new("test-known"),
            AgentType::GeneralPurpose,
            Model::Opus46,
        );
        // Even if override is set, known models use their display_name
        session.model_display_override = Some("something else".to_string());

        let view = SessionView::from_domain(&session);
        assert_eq!(view.model, "Opus 4.6");
    }

    #[test]
    fn test_lines_changed() {
        let lines = LinesChanged::new(150, 30);
        assert_eq!(lines.net(), 120);
        assert_eq!(lines.churn(), 180);
        assert_eq!(lines.format(), "+150 -30");
        assert_eq!(lines.format_net(), "+120");
    }

    #[test]
    fn test_session_duration_formatting() {
        assert_eq!(SessionDuration::from_total_ms(35_000).format(), "35s");
        assert_eq!(SessionDuration::from_total_ms(135_000).format(), "2m 15s");
        assert_eq!(SessionDuration::from_total_ms(5_400_000).format(), "1h 30m");
    }

    #[test]
    fn test_session_id_pending_from_pid() {
        let id = SessionId::pending_from_pid(12345);
        assert_eq!(id.as_str(), "pending-12345");
        assert!(id.is_pending());
        assert_eq!(id.pending_pid(), Some(12345));
    }

    #[test]
    fn test_session_id_is_pending_true() {
        let id = SessionId::new("pending-99999");
        assert!(id.is_pending());
    }

    #[test]
    fn test_session_id_is_pending_false() {
        let id = SessionId::new("8e11bfb5-7dc2-432b-9206-928fa5c35731");
        assert!(!id.is_pending());
    }

    #[test]
    fn test_session_id_pending_pid_returns_none_for_regular_id() {
        let id = SessionId::new("8e11bfb5-7dc2-432b-9206-928fa5c35731");
        assert_eq!(id.pending_pid(), None);
    }

    #[test]
    fn test_session_id_pending_pid_returns_none_for_invalid_pid() {
        let id = SessionId::new("pending-not-a-number");
        assert_eq!(id.pending_pid(), None);
    }

    #[test]
    fn test_apply_hook_event_interactive_tool() {
        let mut session = create_test_session("test-interactive");

        // PreToolUse with interactive tool → AttentionNeeded
        session.apply_hook_event(HookEventType::PreToolUse, Some("AskUserQuestion"));

        assert_eq!(session.status, SessionStatus::AttentionNeeded);
        assert_eq!(
            session
                .current_activity
                .as_ref()
                .map(|a| a.display())
                .as_deref(),
            Some("AskUserQuestion")
        );

        // PostToolUse → back to Working (thinking)
        session.apply_hook_event(HookEventType::PostToolUse, None);
        assert_eq!(session.status, SessionStatus::Working);
    }

    #[test]
    fn test_apply_hook_event_enter_plan_mode() {
        let mut session = create_test_session("test-plan");

        // EnterPlanMode is also interactive
        session.apply_hook_event(HookEventType::PreToolUse, Some("EnterPlanMode"));

        assert_eq!(session.status, SessionStatus::AttentionNeeded);
        assert_eq!(
            session
                .current_activity
                .as_ref()
                .map(|a| a.display())
                .as_deref(),
            Some("EnterPlanMode")
        );
    }

    #[test]
    fn test_apply_hook_event_standard_tool() {
        let mut session = create_test_session("test-standard");

        // PreToolUse with standard tool → Working
        session.apply_hook_event(HookEventType::PreToolUse, Some("Bash"));

        assert_eq!(session.status, SessionStatus::Working);
        assert_eq!(
            session
                .current_activity
                .as_ref()
                .map(|a| a.display())
                .as_deref(),
            Some("Bash")
        );

        // PostToolUse → still Working (thinking)
        session.apply_hook_event(HookEventType::PostToolUse, Some("Bash"));
        assert_eq!(session.status, SessionStatus::Working);
    }

    #[test]
    fn test_apply_hook_event_none_tool_name() {
        let mut session = create_test_session("test-none");
        let original_status = session.status;

        // PreToolUse with None tool name should not change status
        session.apply_hook_event(HookEventType::PreToolUse, None);

        assert_eq!(
            session.status, original_status,
            "PreToolUse with None tool_name should not change status"
        );
    }

    #[test]
    fn test_apply_hook_event_empty_tool_name() {
        let mut session = create_test_session("test-empty");

        // Empty string tool name - should be treated as standard tool
        // (is_interactive_tool returns false for empty strings)
        session.apply_hook_event(HookEventType::PreToolUse, Some(""));

        assert_eq!(session.status, SessionStatus::Working);
    }

    #[test]
    fn test_activity_detail_creation() {
        let detail = ActivityDetail::new("Bash");
        assert_eq!(detail.tool_name.as_deref(), Some("Bash"));
        assert!(detail.started_at <= Utc::now());
        assert!(detail.context.is_none());
    }

    #[test]
    fn test_activity_detail_with_context() {
        let detail = ActivityDetail::with_context("Compacting");
        assert!(detail.tool_name.is_none());
        assert_eq!(detail.context.as_deref(), Some("Compacting"));
    }

    #[test]
    fn test_activity_detail_display() {
        let detail = ActivityDetail::new("Read");
        assert_eq!(detail.display(), "Read");

        let context_detail = ActivityDetail::with_context("Setup");
        assert_eq!(context_detail.display(), "Setup");
    }

    #[test]
    fn test_new_session_status_variants() {
        // All three states should exist
        let idle = SessionStatus::Idle;
        let working = SessionStatus::Working;
        let attention = SessionStatus::AttentionNeeded;

        assert_eq!(idle.label(), "idle");
        assert_eq!(working.label(), "working");
        assert_eq!(attention.label(), "needs input");
    }

    #[test]
    fn test_session_status_should_blink() {
        assert!(!SessionStatus::Idle.should_blink());
        assert!(!SessionStatus::Working.should_blink());
        assert!(SessionStatus::AttentionNeeded.should_blink());
    }

    #[test]
    fn test_session_status_icons() {
        assert_eq!(SessionStatus::Idle.icon(), "-");
        assert_eq!(SessionStatus::Working.icon(), ">");
        assert_eq!(SessionStatus::AttentionNeeded.icon(), "!");
    }

    #[test]
    fn test_session_domain_new_fields_default() {
        let session = create_test_session("test-defaults");
        assert!(session.project_root.is_none());
        assert!(session.worktree_path.is_none());
        assert!(session.worktree_branch.is_none());
        assert!(session.parent_session_id.is_none());
        assert!(session.child_session_ids.is_empty());
    }

    #[test]
    fn test_session_view_includes_new_fields() {
        let mut session = create_test_session("test-view-fields");
        session.project_root = Some("/home/user/project".to_string());
        session.worktree_path = Some("/home/user/worktree".to_string());
        session.worktree_branch = Some("feature-x".to_string());
        session.parent_session_id = Some(SessionId::new("parent-123"));
        session.child_session_ids = vec![
            SessionId::new("child-1"),
            SessionId::new("child-2"),
        ];

        let view = SessionView::from_domain(&session);

        assert_eq!(view.project_root, Some("/home/user/project".to_string()));
        assert_eq!(view.worktree_path, Some("/home/user/worktree".to_string()));
        assert_eq!(view.worktree_branch, Some("feature-x".to_string()));
        assert_eq!(view.parent_session_id, Some(SessionId::new("parent-123")));
        assert_eq!(view.child_session_ids.len(), 2);
        assert_eq!(view.child_session_ids[0].as_str(), "child-1");
        assert_eq!(view.child_session_ids[1].as_str(), "child-2");
    }
}
