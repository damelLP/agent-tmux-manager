# Agent Tmux Manager Domain Model

> **Panic-Free Policy:** All code in this document follows the panic-free guidelines from `CLAUDE.md`.
> No `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, `todo!()`, or direct indexing `[i]` in production code.
> Use `?`, `.ok()`, `.unwrap_or()`, `.unwrap_or_default()`, `.get()`, or pattern matching instead.
> Exception: `.expect()` is allowed for compile-time known-valid literals (documented with SAFETY comment).

## Overview

This document defines the domain model for Agent Tmux Manager, an htop-style monitoring system for Claude Code agents. The model follows Domain-Driven Design (DDD) principles with clear separation between:

- **Domain Layer**: Pure business logic, value objects, and aggregates
- **Infrastructure Layer**: OS/system concerns, I/O, persistence
- **Application Layer**: DTOs and views for clients

---

## Architecture Diagram

```
+-----------------------------------------------------------+
|                     APPLICATION LAYER                       |
|  +-------------------------------------------------------+  |
|  |                    SessionView (DTO)                   |  |
|  |  Read-only snapshot for TUI clients                   |  |
|  +-------------------------------------------------------+  |
+-----------------------------------------------------------+
                              ^
                              | transforms
+-----------------------------------------------------------+
|                      DOMAIN LAYER                          |
|  +------------------+  +------------------+                |
|  | Type-Safe IDs    |  | Value Objects    |                |
|  | - SessionId      |  | - Money          |                |
|  | - ToolUseId      |  | - ContextUsage   |                |
|  | - TranscriptPath |  | - TokenCount     |                |
|  +------------------+  | - Duration       |                |
|                        +------------------+                |
|  +-------------------------------------------------------+  |
|  |                  SessionDomain                         |  |
|  |  - Business state (model, context, cost, status)      |  |
|  |  - Domain invariants and validation                   |  |
|  +-------------------------------------------------------+  |
|  +-------------------------------------------------------+  |
|  |               Domain Services                          |  |
|  |  - SessionAggregator (compute global stats)           |  |
|  |  - CostCalculator (cost formatting/computation)       |  |
|  |  - ContextAnalyzer (usage analysis and warnings)      |  |
|  +-------------------------------------------------------+  |
+-----------------------------------------------------------+
                              ^
                              | uses
+-----------------------------------------------------------+
|                   INFRASTRUCTURE LAYER                     |
|  +-------------------------------------------------------+  |
|  |               SessionInfrastructure                    |  |
|  |  - PID, socket paths, timestamps                      |  |
|  |  - File system references                             |  |
|  |  - System resource tracking                           |  |
|  +-------------------------------------------------------+  |
+-----------------------------------------------------------+
```

---

## Type-Safe Identifiers

Newtype wrappers prevent mixing up different string-based identifiers at compile time.

### SessionId

```rust
use serde::{Deserialize, Serialize};
use std::fmt;

/// Unique identifier for a Claude Code session.
///
/// Wraps a UUID string (e.g., "8e11bfb5-7dc2-432b-9206-928fa5c35731").
/// Obtained from Claude Code's status line JSON `session_id` field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Creates a new SessionId from a string.
    ///
    /// Note: This does not validate UUID format. Claude Code provides
    /// the session_id, so we trust its format.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the underlying string reference.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns a shortened display form (first 8 characters).
    ///
    /// Useful for compact TUI display.
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
```

### ToolUseId

```rust
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
```

### TranscriptPath

```rust
use std::path::{Path, PathBuf};

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
```

---

## Type-Safe Enums

Replace stringly-typed fields with proper enums for compile-time safety.

### Model

```rust
/// Claude model identifier.
///
/// Parsed from status line JSON: `model.id` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Model {
    /// Claude Opus 4.5 (claude-opus-4-5-20251101)
    #[serde(rename = "claude-opus-4-5-20251101")]
    Opus45,

    /// Claude Sonnet 4 (claude-sonnet-4-20250514)
    #[serde(rename = "claude-sonnet-4-20250514")]
    Sonnet4,

    /// Claude Haiku 3.5 (claude-3-5-haiku-20241022)
    #[serde(rename = "claude-3-5-haiku-20241022")]
    Haiku35,

    /// Claude Sonnet 3.5 v2 (claude-3-5-sonnet-20241022)
    #[serde(rename = "claude-3-5-sonnet-20241022")]
    Sonnet35V2,

    /// Unknown or future model
    #[serde(other)]
    Unknown,
}

impl Model {
    /// Returns a human-readable display name.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Opus45 => "Opus 4.5",
            Self::Sonnet4 => "Sonnet 4",
            Self::Haiku35 => "Haiku 3.5",
            Self::Sonnet35V2 => "Sonnet 3.5 v2",
            Self::Unknown => "Unknown",
        }
    }

    /// Returns the context window size for this model.
    pub fn context_window_size(&self) -> u32 {
        match self {
            Self::Opus45 => 200_000,
            Self::Sonnet4 => 200_000,
            Self::Haiku35 => 200_000,
            Self::Sonnet35V2 => 200_000,
            Self::Unknown => 200_000, // Default assumption
        }
    }

    /// Returns approximate cost per million input tokens (USD).
    pub fn input_cost_per_million(&self) -> f64 {
        match self {
            Self::Opus45 => 15.00,
            Self::Sonnet4 => 3.00,
            Self::Haiku35 => 0.80,
            Self::Sonnet35V2 => 3.00,
            Self::Unknown => 3.00, // Conservative default
        }
    }

    /// Returns approximate cost per million output tokens (USD).
    pub fn output_cost_per_million(&self) -> f64 {
        match self {
            Self::Opus45 => 75.00,
            Self::Sonnet4 => 15.00,
            Self::Haiku35 => 4.00,
            Self::Sonnet35V2 => 15.00,
            Self::Unknown => 15.00, // Conservative default
        }
    }
}

impl Default for Model {
    fn default() -> Self {
        Self::Unknown
    }
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name())
    }
}
```

### AgentType

```rust
/// Type of Claude Code agent.
///
/// Claude Code spawns different agent types for different purposes:
/// - Main agent for general tasks
/// - Specialized subagents for exploration, planning, code review
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    /// General-purpose main agent
    GeneralPurpose,

    /// Task/explore subagent for file exploration
    Explore,

    /// Planning subagent for task breakdown
    Plan,

    /// Code review subagent
    CodeReviewer,

    /// File search/analysis subagent
    FileSearch,

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
            Self::Custom(_) => "Custom",
        }
    }
}

impl Default for AgentType {
    fn default() -> Self {
        Self::GeneralPurpose
    }
}

impl fmt::Display for AgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.label())
    }
}
```

### SessionStatus

```rust
/// Current operational status of a session.
///
/// Derived from session activity patterns and hook events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionStatus {
    /// Session is actively processing (received update within 5 seconds)
    Active,

    /// Agent is thinking/generating (between tool calls)
    Thinking,

    /// Agent is executing a tool
    RunningTool {
        /// Name of the tool being executed
        tool_name: String,
        /// When tool execution started
        #[serde(skip_serializing_if = "Option::is_none")]
        started_at: Option<chrono::DateTime<chrono::Utc>>,
    },

    /// Agent is waiting for user permission to execute a tool
    WaitingForPermission {
        /// Tool awaiting permission
        tool_name: String,
    },

    /// Session is idle (no activity for 30-90 seconds)
    Idle,

    /// Session is stale (no activity for >90 seconds, pending cleanup)
    Stale,
}

impl SessionStatus {
    /// Returns true if the session is in an active state.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Active | Self::Thinking | Self::RunningTool { .. }
        )
    }

    /// Returns true if the session is waiting for user input.
    pub fn needs_attention(&self) -> bool {
        matches!(self, Self::WaitingForPermission { .. })
    }

    /// Returns true if the session may be cleaned up.
    pub fn is_removable(&self) -> bool {
        matches!(self, Self::Stale)
    }

    /// Returns a short status label for display.
    pub fn label(&self) -> &str {
        match self {
            Self::Active => "active",
            Self::Thinking => "thinking",
            Self::RunningTool { .. } => "running",
            Self::WaitingForPermission { .. } => "waiting",
            Self::Idle => "idle",
            Self::Stale => "stale",
        }
    }

    /// Returns the tool name if applicable.
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            Self::RunningTool { tool_name, .. } => Some(tool_name.as_str()),
            Self::WaitingForPermission { tool_name } => Some(tool_name.as_str()),
            _ => None,
        }
    }
}

impl Default for SessionStatus {
    fn default() -> Self {
        Self::Active
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => write!(f, "Active"),
            Self::Thinking => write!(f, "Thinking..."),
            Self::RunningTool { tool_name, .. } => write!(f, "Running: {}", tool_name),
            Self::WaitingForPermission { tool_name } => {
                write!(f, "Permission: {}", tool_name)
            }
            Self::Idle => write!(f, "Idle"),
            Self::Stale => write!(f, "Stale"),
        }
    }
}
```

### HookEventType

```rust
/// Types of hook events from Claude Code.
///
/// Based on validated Claude Code hook documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum HookEventType {
    /// Before a tool is executed (can be used for permission checks)
    PreToolUse,

    /// After a tool completes execution
    PostToolUse,

    /// When a new session starts (not currently used, future)
    SessionStart,

    /// When a session ends (not currently used, future)
    SessionEnd,

    /// Notification event (informational)
    Notification,
}

impl HookEventType {
    /// Returns true if this is a pre-execution event.
    pub fn is_pre_event(&self) -> bool {
        matches!(self, Self::PreToolUse | Self::SessionStart)
    }

    /// Returns true if this is a post-execution event.
    pub fn is_post_event(&self) -> bool {
        matches!(self, Self::PostToolUse | Self::SessionEnd)
    }
}

impl fmt::Display for HookEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PreToolUse => write!(f, "PreToolUse"),
            Self::PostToolUse => write!(f, "PostToolUse"),
            Self::SessionStart => write!(f, "SessionStart"),
            Self::SessionEnd => write!(f, "SessionEnd"),
            Self::Notification => write!(f, "Notification"),
        }
    }
}
```

---

## Value Objects

Immutable objects defined by their attributes. Used for validated domain data.

### Money

```rust
/// Represents a monetary amount in USD.
///
/// Internally stored as microdollars (millionths of a dollar) for precision.
/// Avoids floating-point errors in cost accumulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Money {
    /// Amount in microdollars (1 USD = 1,000,000 microdollars)
    microdollars: i64,
}

impl Money {
    /// One dollar in microdollars.
    const MICRODOLLARS_PER_DOLLAR: i64 = 1_000_000;

    /// Creates Money from a USD dollar amount.
    pub fn from_usd(dollars: f64) -> Self {
        let microdollars = (dollars * Self::MICRODOLLARS_PER_DOLLAR as f64).round() as i64;
        Self { microdollars }
    }

    /// Creates Money from microdollars.
    pub fn from_microdollars(microdollars: i64) -> Self {
        Self { microdollars }
    }

    /// Creates a zero Money value.
    pub const fn zero() -> Self {
        Self { microdollars: 0 }
    }

    /// Returns the amount in USD as a float.
    pub fn as_usd(&self) -> f64 {
        self.microdollars as f64 / Self::MICRODOLLARS_PER_DOLLAR as f64
    }

    /// Returns the amount in microdollars.
    pub fn as_microdollars(&self) -> i64 {
        self.microdollars
    }

    /// Returns true if the amount is zero.
    pub fn is_zero(&self) -> bool {
        self.microdollars == 0
    }

    /// Adds another Money value.
    pub fn add(&self, other: Money) -> Self {
        Self {
            microdollars: self.microdollars.saturating_add(other.microdollars),
        }
    }

    /// Formats the amount for display.
    ///
    /// Returns format like "$0.35", "$1.50", "$12.34"
    pub fn format(&self) -> String {
        let dollars = self.as_usd();
        if dollars < 0.01 && dollars > 0.0 {
            format!("${:.4}", dollars)
        } else if dollars < 10.0 {
            format!("${:.2}", dollars)
        } else if dollars < 100.0 {
            format!("${:.1}", dollars)
        } else {
            format!("${:.0}", dollars)
        }
    }

    /// Formats the amount compactly for narrow displays.
    ///
    /// Returns format like "35c", "$1.5", "$12"
    pub fn format_compact(&self) -> String {
        let dollars = self.as_usd();
        if dollars < 0.01 && dollars > 0.0 {
            let cents = dollars * 100.0;
            format!("{:.1}c", cents)
        } else if dollars < 1.0 {
            let cents = (dollars * 100.0).round() as i32;
            format!("{}c", cents)
        } else if dollars < 10.0 {
            format!("${:.1}", dollars)
        } else {
            format!("${:.0}", dollars)
        }
    }
}

impl std::ops::Add for Money {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            microdollars: self.microdollars.saturating_add(other.microdollars),
        }
    }
}

impl std::ops::AddAssign for Money {
    fn add_assign(&mut self, other: Self) {
        self.microdollars = self.microdollars.saturating_add(other.microdollars);
    }
}

impl Serialize for Money {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Serialize as USD float for JSON compatibility
        serializer.serialize_f64(self.as_usd())
    }
}

impl<'de> Deserialize<'de> for Money {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let dollars = f64::deserialize(deserializer)?;
        Ok(Money::from_usd(dollars))
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format())
    }
}

#[cfg(test)]
mod money_tests {
    use super::*;

    #[test]
    fn test_money_precision() {
        let a = Money::from_usd(0.001);
        let b = Money::from_usd(0.001);
        let sum = a + b;
        assert_eq!(sum.as_usd(), 0.002);
    }

    #[test]
    fn test_money_formatting() {
        assert_eq!(Money::from_usd(0.005).format(), "$0.0050");
        assert_eq!(Money::from_usd(0.35).format(), "$0.35");
        assert_eq!(Money::from_usd(1.50).format(), "$1.50");
        assert_eq!(Money::from_usd(12.34).format(), "$12.3");
        assert_eq!(Money::from_usd(150.00).format(), "$150");
    }

    #[test]
    fn test_money_compact_formatting() {
        assert_eq!(Money::from_usd(0.35).format_compact(), "35c");
        assert_eq!(Money::from_usd(1.50).format_compact(), "$1.5");
        assert_eq!(Money::from_usd(12.34).format_compact(), "$12");
    }
}
```

### TokenCount

```rust
/// Represents a count of tokens.
///
/// Used for input tokens, output tokens, cache tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenCount(u64);

impl TokenCount {
    /// Creates a new TokenCount.
    pub const fn new(count: u64) -> Self {
        Self(count)
    }

    /// Creates a zero TokenCount.
    pub const fn zero() -> Self {
        Self(0)
    }

    /// Returns the raw count.
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    /// Returns true if count is zero.
    pub const fn is_zero(&self) -> bool {
        self.0 == 0
    }

    /// Formats the token count for display.
    ///
    /// Uses K/M suffixes for large numbers.
    pub fn format(&self) -> String {
        if self.0 < 1_000 {
            format!("{}", self.0)
        } else if self.0 < 10_000 {
            format!("{:.1}K", self.0 as f64 / 1_000.0)
        } else if self.0 < 1_000_000 {
            format!("{}K", self.0 / 1_000)
        } else {
            format!("{:.1}M", self.0 as f64 / 1_000_000.0)
        }
    }

    /// Saturating addition.
    pub fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

impl std::ops::Add for TokenCount {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

impl std::ops::AddAssign for TokenCount {
    fn add_assign(&mut self, other: Self) {
        self.0 = self.0.saturating_add(other.0);
    }
}

impl From<u64> for TokenCount {
    fn from(n: u64) -> Self {
        Self(n)
    }
}

impl From<u32> for TokenCount {
    fn from(n: u32) -> Self {
        Self(n as u64)
    }
}

impl fmt::Display for TokenCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format())
    }
}
```

### ContextUsage

```rust
/// Context window usage information.
///
/// Tracks token counts and calculates usage percentage.
/// Based on validated Claude Code status line JSON structure.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ContextUsage {
    /// Total input tokens across all turns
    pub total_input_tokens: TokenCount,

    /// Total output tokens across all turns
    pub total_output_tokens: TokenCount,

    /// Maximum context window size for the model
    pub context_window_size: u32,

    /// Current turn's input tokens
    pub current_input_tokens: TokenCount,

    /// Current turn's output tokens
    pub current_output_tokens: TokenCount,

    /// Tokens written to cache this turn
    pub cache_creation_tokens: TokenCount,

    /// Tokens read from cache this turn
    pub cache_read_tokens: TokenCount,
}

impl ContextUsage {
    /// Creates a new ContextUsage with default values.
    pub fn new(context_window_size: u32) -> Self {
        Self {
            context_window_size,
            ..Default::default()
        }
    }

    /// Calculates the total tokens used (approximation for context usage).
    ///
    /// Context is primarily consumed by input tokens (including cached).
    pub fn total_tokens(&self) -> TokenCount {
        self.total_input_tokens
            .saturating_add(self.total_output_tokens)
    }

    /// Returns the percentage of context window used (0.0 to 100.0).
    ///
    /// Based on total input tokens vs context window size.
    pub fn usage_percentage(&self) -> f64 {
        if self.context_window_size == 0 {
            return 0.0;
        }
        let usage = self.total_input_tokens.as_u64() as f64 / self.context_window_size as f64;
        (usage * 100.0).min(100.0)
    }

    /// Returns true if context usage is above the warning threshold (80%).
    pub fn is_warning(&self) -> bool {
        self.usage_percentage() >= 80.0
    }

    /// Returns true if context usage is critical (>90%).
    pub fn is_critical(&self) -> bool {
        self.usage_percentage() >= 90.0
    }

    /// Returns true if exceeds 200K tokens (Claude Code's extended context marker).
    pub fn exceeds_200k(&self) -> bool {
        self.total_input_tokens.as_u64() > 200_000
    }

    /// Returns the remaining tokens before hitting context limit.
    pub fn remaining_tokens(&self) -> TokenCount {
        let used = self.total_input_tokens.as_u64();
        let limit = self.context_window_size as u64;
        TokenCount::new(limit.saturating_sub(used))
    }

    /// Formats usage for display (e.g., "45.2% (5.1K/200K)").
    pub fn format(&self) -> String {
        format!(
            "{:.1}% ({}/{})",
            self.usage_percentage(),
            self.total_input_tokens.format(),
            TokenCount::new(self.context_window_size as u64).format()
        )
    }

    /// Formats usage compactly (e.g., "45%").
    pub fn format_compact(&self) -> String {
        format!("{:.0}%", self.usage_percentage())
    }
}

impl fmt::Display for ContextUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format())
    }
}

#[cfg(test)]
mod context_usage_tests {
    use super::*;

    #[test]
    fn test_usage_percentage() {
        let usage = ContextUsage {
            total_input_tokens: TokenCount::new(100_000),
            context_window_size: 200_000,
            ..Default::default()
        };
        assert!((usage.usage_percentage() - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_warning_thresholds() {
        let normal = ContextUsage {
            total_input_tokens: TokenCount::new(100_000),
            context_window_size: 200_000,
            ..Default::default()
        };
        assert!(!normal.is_warning());
        assert!(!normal.is_critical());

        let warning = ContextUsage {
            total_input_tokens: TokenCount::new(160_000),
            context_window_size: 200_000,
            ..Default::default()
        };
        assert!(warning.is_warning());
        assert!(!warning.is_critical());

        let critical = ContextUsage {
            total_input_tokens: TokenCount::new(190_000),
            context_window_size: 200_000,
            ..Default::default()
        };
        assert!(critical.is_warning());
        assert!(critical.is_critical());
    }
}
```

### Duration (Session Duration)

```rust
use chrono::{DateTime, Utc};

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
        Self { total_ms, api_ms: 0 }
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
            format!("{}s", secs)
        } else if secs < 3600 {
            let mins = secs / 60;
            let remaining_secs = secs % 60;
            if remaining_secs == 0 {
                format!("{}m", mins)
            } else {
                format!("{}m {}s", mins, remaining_secs)
            }
        } else {
            let hours = secs / 3600;
            let remaining_mins = (secs % 3600) / 60;
            if remaining_mins == 0 {
                format!("{}h", hours)
            } else {
                format!("{}h {}m", hours, remaining_mins)
            }
        }
    }

    /// Formats duration compactly.
    pub fn format_compact(&self) -> String {
        let secs = self.total_ms / 1000;
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m", secs / 60)
        } else {
            format!("{}h", secs / 3600)
        }
    }
}

impl fmt::Display for SessionDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format())
    }
}
```

### LinesChanged

```rust
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
            format!("+{}", net)
        } else {
            format!("{}", net)
        }
    }
}

impl fmt::Display for LinesChanged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format())
    }
}
```

---

## Domain Entities

### SessionDomain

The core domain entity containing pure business logic.

```rust
use chrono::{DateTime, Utc};

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

    /// Current session status
    pub status: SessionStatus,

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
}

impl SessionDomain {
    /// Creates a new SessionDomain with required fields.
    pub fn new(id: SessionId, agent_type: AgentType, model: Model) -> Self {
        let now = Utc::now();
        Self {
            id,
            agent_type,
            model,
            status: SessionStatus::Active,
            context: ContextUsage::new(model.context_window_size()),
            cost: Money::zero(),
            duration: SessionDuration::default(),
            lines_changed: LinesChanged::default(),
            started_at: now,
            last_activity: now,
            working_directory: None,
            claude_code_version: None,
        }
    }

    /// Creates a SessionDomain from Claude Code status line JSON.
    pub fn from_status_line(
        session_id: &str,
        model_id: &str,
        cost_usd: f64,
        total_duration_ms: u64,
        api_duration_ms: u64,
        lines_added: u64,
        lines_removed: u64,
        total_input_tokens: u64,
        total_output_tokens: u64,
        context_window_size: u32,
        current_input_tokens: u64,
        current_output_tokens: u64,
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
        cwd: Option<&str>,
        version: Option<&str>,
    ) -> Self {
        let model = serde_json::from_str(&format!("\"{}\"", model_id))
            .unwrap_or(Model::Unknown);

        let mut session = Self::new(
            SessionId::new(session_id),
            AgentType::GeneralPurpose, // Default, may be updated by hook events
            model,
        );

        session.cost = Money::from_usd(cost_usd);
        session.duration = SessionDuration::new(total_duration_ms, api_duration_ms);
        session.lines_changed = LinesChanged::new(lines_added, lines_removed);
        session.context = ContextUsage {
            total_input_tokens: TokenCount::new(total_input_tokens),
            total_output_tokens: TokenCount::new(total_output_tokens),
            context_window_size,
            current_input_tokens: TokenCount::new(current_input_tokens),
            current_output_tokens: TokenCount::new(current_output_tokens),
            cache_creation_tokens: TokenCount::new(cache_creation_tokens),
            cache_read_tokens: TokenCount::new(cache_read_tokens),
        };
        session.working_directory = cwd.map(|s| s.to_string());
        session.claude_code_version = version.map(|s| s.to_string());
        session.last_activity = Utc::now();

        session
    }

    /// Updates the session with new status line data.
    pub fn update_from_status_line(
        &mut self,
        cost_usd: f64,
        total_duration_ms: u64,
        api_duration_ms: u64,
        lines_added: u64,
        lines_removed: u64,
        total_input_tokens: u64,
        total_output_tokens: u64,
        current_input_tokens: u64,
        current_output_tokens: u64,
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
    ) {
        self.cost = Money::from_usd(cost_usd);
        self.duration = SessionDuration::new(total_duration_ms, api_duration_ms);
        self.lines_changed = LinesChanged::new(lines_added, lines_removed);
        self.context.total_input_tokens = TokenCount::new(total_input_tokens);
        self.context.total_output_tokens = TokenCount::new(total_output_tokens);
        self.context.current_input_tokens = TokenCount::new(current_input_tokens);
        self.context.current_output_tokens = TokenCount::new(current_output_tokens);
        self.context.cache_creation_tokens = TokenCount::new(cache_creation_tokens);
        self.context.cache_read_tokens = TokenCount::new(cache_read_tokens);
        self.last_activity = Utc::now();

        // Update status based on activity
        if !matches!(self.status, SessionStatus::WaitingForPermission { .. }) {
            self.status = SessionStatus::Active;
        }
    }

    /// Updates status based on a hook event.
    pub fn apply_hook_event(&mut self, event_type: HookEventType, tool_name: Option<&str>) {
        self.last_activity = Utc::now();

        match event_type {
            HookEventType::PreToolUse => {
                if let Some(name) = tool_name {
                    self.status = SessionStatus::RunningTool {
                        tool_name: name.to_string(),
                        started_at: Some(Utc::now()),
                    };
                }
            }
            HookEventType::PostToolUse => {
                self.status = SessionStatus::Thinking;
            }
            _ => {}
        }
    }

    /// Marks the session as waiting for permission.
    pub fn set_waiting_for_permission(&mut self, tool_name: &str) {
        self.status = SessionStatus::WaitingForPermission {
            tool_name: tool_name.to_string(),
        };
        self.last_activity = Utc::now();
    }

    /// Returns the session age (time since started).
    pub fn age(&self) -> chrono::Duration {
        Utc::now().signed_duration_since(self.started_at)
    }

    /// Returns time since last activity.
    pub fn time_since_activity(&self) -> chrono::Duration {
        Utc::now().signed_duration_since(self.last_activity)
    }

    /// Returns true if the session should be considered stale.
    ///
    /// A session is stale if no activity for 90 seconds.
    pub fn is_stale(&self) -> bool {
        self.time_since_activity() > chrono::Duration::seconds(90)
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
```

### SessionInfrastructure

Infrastructure concerns separated from domain logic.

```rust
use std::collections::VecDeque;
use std::path::PathBuf;

/// Infrastructure-level data for a session.
///
/// Contains OS/system concerns that don't belong in the domain model.
/// Owned by RegistryActor alongside SessionDomain.
#[derive(Debug, Clone)]
pub struct SessionInfrastructure {
    /// Process ID of the Claude Code process (if known)
    pub pid: Option<u32>,

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
            socket_path: None,
            transcript_path: None,
            recent_tools: VecDeque::with_capacity(Self::MAX_TOOL_HISTORY),
            update_count: 0,
            hook_event_count: 0,
            last_error: None,
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

impl Default for SessionInfrastructure {
    fn default() -> Self {
        Self::new()
    }
}

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
```

---

## Application Layer DTOs

### SessionView

Read-only DTO for TUI clients.

```rust
/// Read-only view of a session for TUI display.
///
/// Immutable snapshot created from SessionDomain.
/// Implements Clone for easy distribution to multiple UI components.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    /// Session identifier
    pub id: SessionId,

    /// Short ID for display (first 8 chars)
    pub id_short: String,

    /// Agent type label
    pub agent_type: String,

    /// Model display name
    pub model: String,

    /// Status label
    pub status: String,

    /// Status detail (tool name if applicable)
    pub status_detail: Option<String>,

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

    /// Whether session is stale
    pub is_stale: bool,

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
            model: session.model.display_name().to_string(),
            status: session.status.label().to_string(),
            status_detail: session.status.tool_name().map(|s| s.to_string()),
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
            is_stale: session.is_stale(),
            needs_attention: session.status.needs_attention() || session.needs_context_attention(),
            last_activity_display: format_duration(since_activity),
            age_display: format_duration(age),
            started_at: session.started_at.to_rfc3339(),
            last_activity: session.last_activity.to_rfc3339(),
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
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}
```

---

## Domain Services

### SessionAggregator

Computes global statistics across all sessions.

```rust
/// Aggregator for computing global statistics across sessions.
///
/// Stateless service that operates on collections of SessionDomain/SessionView.
pub struct SessionAggregator;

impl SessionAggregator {
    /// Computes aggregate statistics for a collection of sessions.
    pub fn aggregate(sessions: &[SessionView]) -> AggregateStats {
        let total_sessions = sessions.len();
        let active_sessions = sessions.iter().filter(|s| !s.is_stale).count();
        let stale_sessions = sessions.iter().filter(|s| s.is_stale).count();
        let attention_needed = sessions.iter().filter(|s| s.needs_attention).count();

        let total_cost = sessions.iter().map(|s| s.cost_usd).sum::<f64>();
        let total_duration_secs = sessions.iter().map(|s| s.duration_seconds).sum::<f64>();

        let avg_context = if total_sessions > 0 {
            sessions.iter().map(|s| s.context_percentage).sum::<f64>() / total_sessions as f64
        } else {
            0.0
        };

        let max_context = sessions
            .iter()
            .map(|s| s.context_percentage)
            .fold(0.0_f64, f64::max);

        let high_context_sessions = sessions
            .iter()
            .filter(|s| s.context_warning || s.context_critical)
            .count();

        AggregateStats {
            total_sessions,
            active_sessions,
            stale_sessions,
            attention_needed,
            total_cost: Money::from_usd(total_cost),
            total_duration: SessionDuration::from_total_ms((total_duration_secs * 1000.0) as u64),
            avg_context_percentage: avg_context,
            max_context_percentage: max_context,
            high_context_sessions,
        }
    }

    /// Groups sessions by agent type.
    pub fn group_by_agent_type(sessions: &[SessionView]) -> std::collections::HashMap<String, Vec<&SessionView>> {
        let mut groups: std::collections::HashMap<String, Vec<&SessionView>> = std::collections::HashMap::new();

        for session in sessions {
            groups
                .entry(session.agent_type.clone())
                .or_default()
                .push(session);
        }

        groups
    }

    /// Returns sessions sorted by context usage (highest first).
    pub fn sort_by_context_usage(sessions: &mut [SessionView]) {
        sessions.sort_by(|a, b| {
            b.context_percentage
                .partial_cmp(&a.context_percentage)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    /// Returns sessions sorted by cost (highest first).
    pub fn sort_by_cost(sessions: &mut [SessionView]) {
        sessions.sort_by(|a, b| {
            b.cost_usd
                .partial_cmp(&a.cost_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    /// Returns sessions sorted by last activity (most recent first).
    pub fn sort_by_activity(sessions: &mut [SessionView]) {
        sessions.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
    }
}

/// Aggregate statistics across all sessions.
#[derive(Debug, Clone, Default)]
pub struct AggregateStats {
    /// Total number of sessions
    pub total_sessions: usize,

    /// Number of active (non-stale) sessions
    pub active_sessions: usize,

    /// Number of stale sessions
    pub stale_sessions: usize,

    /// Number of sessions needing attention
    pub attention_needed: usize,

    /// Total cost across all sessions
    pub total_cost: Money,

    /// Total duration across all sessions
    pub total_duration: SessionDuration,

    /// Average context usage percentage
    pub avg_context_percentage: f64,

    /// Maximum context usage percentage
    pub max_context_percentage: f64,

    /// Number of sessions with high context usage
    pub high_context_sessions: usize,
}

impl AggregateStats {
    /// Formats stats for TUI header display.
    pub fn format_header(&self) -> String {
        format!(
            "Sessions: {} ({} active) | Cost: {} | Avg Context: {:.0}%",
            self.total_sessions,
            self.active_sessions,
            self.total_cost.format(),
            self.avg_context_percentage
        )
    }
}
```

### CostCalculator

Cost computation and formatting service.

```rust
/// Service for cost-related calculations.
pub struct CostCalculator;

impl CostCalculator {
    /// Estimates cost based on token counts and model.
    pub fn estimate_cost(
        model: Model,
        input_tokens: TokenCount,
        output_tokens: TokenCount,
    ) -> Money {
        let input_cost =
            input_tokens.as_u64() as f64 * model.input_cost_per_million() / 1_000_000.0;
        let output_cost =
            output_tokens.as_u64() as f64 * model.output_cost_per_million() / 1_000_000.0;

        Money::from_usd(input_cost + output_cost)
    }

    /// Calculates cost rate (cost per hour based on session duration).
    pub fn hourly_rate(cost: Money, duration: SessionDuration) -> Option<Money> {
        let hours = duration.total_seconds() / 3600.0;
        if hours < 0.001 {
            return None; // Too short to calculate meaningful rate
        }
        Some(Money::from_usd(cost.as_usd() / hours))
    }

    /// Formats cost with rate information.
    pub fn format_with_rate(cost: Money, duration: SessionDuration) -> String {
        match Self::hourly_rate(cost, duration) {
            Some(rate) => format!("{} ({}/hr)", cost.format(), rate.format()),
            None => cost.format(),
        }
    }

    /// Calculates projected cost if session continues at current rate.
    pub fn project_cost(
        current_cost: Money,
        current_duration: SessionDuration,
        target_duration_hours: f64,
    ) -> Option<Money> {
        let current_hours = current_duration.total_seconds() / 3600.0;
        if current_hours < 0.001 {
            return None;
        }
        let rate = current_cost.as_usd() / current_hours;
        Some(Money::from_usd(rate * target_duration_hours))
    }
}
```

### ContextAnalyzer

Context usage analysis and warning generation.

```rust
/// Service for analyzing context usage and generating warnings.
pub struct ContextAnalyzer;

/// Warning level for context usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContextWarningLevel {
    /// No warning needed
    Normal,
    /// Usage is elevated but not critical (60-80%)
    Elevated,
    /// Usage is high, should consider compacting (80-90%)
    Warning,
    /// Usage is critical, action needed (>90%)
    Critical,
}

impl ContextAnalyzer {
    /// Analyzes context usage and returns warning level.
    pub fn analyze(context: &ContextUsage) -> ContextWarningLevel {
        let percentage = context.usage_percentage();
        if percentage >= 90.0 {
            ContextWarningLevel::Critical
        } else if percentage >= 80.0 {
            ContextWarningLevel::Warning
        } else if percentage >= 60.0 {
            ContextWarningLevel::Elevated
        } else {
            ContextWarningLevel::Normal
        }
    }

    /// Generates a warning message if applicable.
    pub fn warning_message(context: &ContextUsage) -> Option<String> {
        match Self::analyze(context) {
            ContextWarningLevel::Critical => Some(format!(
                "CRITICAL: Context at {:.0}%. Consider /compact or starting new conversation.",
                context.usage_percentage()
            )),
            ContextWarningLevel::Warning => Some(format!(
                "Warning: Context at {:.0}%. Approaching limit.",
                context.usage_percentage()
            )),
            ContextWarningLevel::Elevated => Some(format!(
                "Note: Context at {:.0}%.",
                context.usage_percentage()
            )),
            ContextWarningLevel::Normal => None,
        }
    }

    /// Estimates remaining "turns" based on average token usage per turn.
    pub fn estimate_remaining_turns(
        context: &ContextUsage,
        avg_tokens_per_turn: u64,
    ) -> Option<u64> {
        if avg_tokens_per_turn == 0 {
            return None;
        }
        let remaining = context.remaining_tokens().as_u64();
        Some(remaining / avg_tokens_per_turn)
    }

    /// Calculates cache efficiency (cache reads vs total input).
    pub fn cache_efficiency(context: &ContextUsage) -> f64 {
        let total_input = context.total_input_tokens.as_u64();
        if total_input == 0 {
            return 0.0;
        }
        let cache_reads = context.cache_read_tokens.as_u64();
        (cache_reads as f64 / total_input as f64) * 100.0
    }
}
```

---

## Error Types

Domain-specific errors following the panic-free policy.

```rust
use thiserror::Error;

/// Errors that can occur in domain operations.
#[derive(Error, Debug, Clone)]
pub enum DomainError {
    /// Session not found in registry
    #[error("Session not found: {session_id}")]
    SessionNotFound { session_id: SessionId },

    /// Session already exists
    #[error("Session already exists: {session_id}")]
    SessionAlreadyExists { session_id: SessionId },

    /// Invalid field value
    #[error("Invalid {field}: {value} (expected {expected})")]
    InvalidFieldValue {
        field: String,
        value: String,
        expected: String,
    },

    /// Parse error for incoming data
    #[error("Failed to parse {field}: {reason}")]
    ParseError { field: String, reason: String },
}

/// Result type for domain operations.
pub type DomainResult<T> = Result<T, DomainError>;
```

---

## Parsing Claude Code Data

Helpers for parsing validated Claude Code JSON structures.

```rust
use serde::Deserialize;

/// Raw status line JSON structure from Claude Code.
///
/// Based on validated integration testing (Week 1).
#[derive(Debug, Clone, Deserialize)]
pub struct RawStatusLine {
    pub session_id: String,
    pub transcript_path: Option<String>,
    pub cwd: Option<String>,
    pub model: RawModel,
    pub workspace: Option<RawWorkspace>,
    pub version: Option<String>,
    pub cost: RawCost,
    pub context_window: RawContextWindow,
    pub exceeds_200k_tokens: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawModel {
    pub id: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawWorkspace {
    pub current_dir: Option<String>,
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

/// Raw hook event JSON structure from Claude Code.
#[derive(Debug, Clone, Deserialize)]
pub struct RawHookEvent {
    pub session_id: String,
    pub hook_event_name: String,
    pub tool_name: Option<String>,
    pub tool_input: Option<serde_json::Value>,
    pub tool_use_id: Option<String>,
}

impl RawStatusLine {
    /// Converts to SessionDomain.
    pub fn to_session_domain(&self) -> SessionDomain {
        let current = self.context_window.current_usage.as_ref();

        SessionDomain::from_status_line(
            &self.session_id,
            &self.model.id,
            self.cost.total_cost_usd,
            self.cost.total_duration_ms,
            self.cost.total_api_duration_ms,
            self.cost.total_lines_added,
            self.cost.total_lines_removed,
            self.context_window.total_input_tokens,
            self.context_window.total_output_tokens,
            self.context_window.context_window_size,
            current.map(|c| c.input_tokens).unwrap_or(0),
            current.map(|c| c.output_tokens).unwrap_or(0),
            current.map(|c| c.cache_creation_input_tokens).unwrap_or(0),
            current.map(|c| c.cache_read_input_tokens).unwrap_or(0),
            self.cwd.as_deref(),
            self.version.as_deref(),
        )
    }
}

impl RawHookEvent {
    /// Parses the hook event type.
    pub fn event_type(&self) -> Option<HookEventType> {
        match self.hook_event_name.as_str() {
            "PreToolUse" => Some(HookEventType::PreToolUse),
            "PostToolUse" => Some(HookEventType::PostToolUse),
            "Notification" => Some(HookEventType::Notification),
            _ => None,
        }
    }
}
```

---

## Module Organization

### Workspace Structure (Multi-Crate)

```
atm/
  Cargo.toml                 # Workspace manifest

  crates/
    atm-core/          # Shared types and logic
      src/
        lib.rs
        session.rs           # Session, SessionId, SessionStatus
        context.rs           # ContextUsage, TokenCount, context analysis
        cost.rs              # Money, cost tracking and formatting
        model.rs             # Model enum, pricing, context limits
        agent.rs             # AgentType enum
        hook.rs              # HookEventType, tool use tracking
        error.rs             # Error types

    atm-protocol/      # Wire protocol (optional, or in core)
      src/
        lib.rs
        message.rs           # ClientMessage, DaemonMessage
        parse.rs             # RawStatusLine, RawHookEvent parsing
        version.rs           # Protocol versioning

    atmd/              # Daemon binary
      src/
        main.rs
        server.rs            # Unix socket server
        registry.rs          # Session registry (actor)
        broadcast.rs         # Client event broadcasting
        cleanup.rs           # Stale session cleanup

    atm/               # TUI binary
      src/
        main.rs
        app.rs               # Application state
        ui/
          mod.rs
          layout.rs          # Split-pane layout
          session_list.rs    # Session list widget
          detail_panel.rs    # Session detail widget
          status_bar.rs      # Connection status, help
        input.rs             # Keyboard handling
        client.rs            # Daemon connection
```

### Why This Structure

- **Descriptive names**: `session.rs`, `context.rs`, `cost.rs` say what they contain
- **Separation by concern**: Each module handles one concept
- **Multi-crate benefits**:
  - `atm-core` compiles once, shared by daemon and TUI
  - Faster incremental builds
  - Clear dependency boundaries
  - Can publish core types independently

### atm-core/src/lib.rs

```rust
//! Agent Tmux Manager Core - Shared types for Claude Code monitoring
//!
//! This crate provides the core domain types shared between
//! the daemon (atmd) and TUI (atm).

pub mod session;
pub mod context;
pub mod cost;
pub mod model;
pub mod agent;
pub mod hook;
pub mod error;

// Re-exports for convenience
pub use session::{Session, SessionId, SessionStatus};
pub use context::{ContextUsage, TokenCount};
pub use cost::Money;
pub use model::Model;
pub use agent::AgentType;
pub use hook::HookEventType;
pub use error::{Error, Result};
```

### Crate Dependencies

```toml
# Cargo.toml (workspace root)
[workspace]
resolver = "2"
members = [
    "crates/atm-core",
    "crates/atm-protocol",
    "crates/atmd",
    "crates/atm",
]

# crates/atmd/Cargo.toml
[dependencies]
atm-core = { path = "../atm-core" }
atm-protocol = { path = "../atm-protocol" }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"

# crates/atm/Cargo.toml (TUI)
[dependencies]
atm-core = { path = "../atm-core" }
atm-protocol = { path = "../atm-protocol" }
ratatui = "0.26"
crossterm = "0.27"
tokio = { version = "1", features = ["full"] }
```

---

## Testing

### Unit Test Examples

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_id_short() {
        let id = SessionId::new("8e11bfb5-7dc2-432b-9206-928fa5c35731");
        assert_eq!(id.short(), "8e11bfb5");
    }

    #[test]
    fn test_model_parsing() {
        let model: Model = serde_json::from_str("\"claude-opus-4-5-20251101\"").unwrap();
        assert_eq!(model, Model::Opus45);
        assert_eq!(model.display_name(), "Opus 4.5");
    }

    #[test]
    fn test_session_status_display() {
        let status = SessionStatus::RunningTool {
            tool_name: "Bash".to_string(),
            started_at: None,
        };
        assert_eq!(format!("{}", status), "Running: Bash");
    }

    #[test]
    fn test_context_usage_warnings() {
        let normal = ContextUsage {
            total_input_tokens: TokenCount::new(50_000),
            context_window_size: 200_000,
            ..Default::default()
        };
        assert_eq!(ContextAnalyzer::analyze(&normal), ContextWarningLevel::Normal);

        let critical = ContextUsage {
            total_input_tokens: TokenCount::new(185_000),
            context_window_size: 200_000,
            ..Default::default()
        };
        assert_eq!(ContextAnalyzer::analyze(&critical), ContextWarningLevel::Critical);
    }

    #[test]
    fn test_aggregate_stats() {
        let sessions = vec![
            SessionView {
                cost_usd: 0.50,
                duration_seconds: 120.0,
                context_percentage: 30.0,
                is_stale: false,
                needs_attention: false,
                context_warning: false,
                context_critical: false,
                // ... other fields with defaults
                ..Default::default()
            },
            SessionView {
                cost_usd: 0.75,
                duration_seconds: 180.0,
                context_percentage: 85.0,
                is_stale: false,
                needs_attention: true,
                context_warning: true,
                context_critical: false,
                ..Default::default()
            },
        ];

        let stats = SessionAggregator::aggregate(&sessions);
        assert_eq!(stats.total_sessions, 2);
        assert_eq!(stats.active_sessions, 2);
        assert_eq!(stats.high_context_sessions, 1);
        assert!((stats.total_cost.as_usd() - 1.25).abs() < 0.001);
    }

    #[test]
    fn test_raw_status_line_parsing() {
        let json = r#"{
            "session_id": "test-123",
            "model": {"id": "claude-opus-4-5-20251101", "display_name": "Opus 4.5"},
            "cost": {"total_cost_usd": 0.35, "total_duration_ms": 35000},
            "context_window": {"total_input_tokens": 5000, "context_window_size": 200000}
        }"#;

        let raw: RawStatusLine = serde_json::from_str(json).unwrap();
        let session = raw.to_session_domain();

        assert_eq!(session.id.as_str(), "test-123");
        assert_eq!(session.model, Model::Opus45);
        assert!((session.cost.as_usd() - 0.35).abs() < 0.001);
    }
}
```

---

## Summary

| Component | Purpose | Key Types |
|-----------|---------|-----------|
| **Identifiers** | Type-safe string wrappers | `SessionId`, `ToolUseId`, `TranscriptPath` |
| **Enums** | Replace stringly-typed fields | `Model`, `AgentType`, `SessionStatus`, `HookEventType` |
| **Value Objects** | Immutable validated data | `Money`, `TokenCount`, `ContextUsage`, `SessionDuration`, `LinesChanged` |
| **Domain Entity** | Core business logic | `SessionDomain` |
| **Infrastructure** | OS/system concerns | `SessionInfrastructure`, `ToolUsageRecord` |
| **View DTO** | Read-only client data | `SessionView` |
| **Services** | Stateless computations | `SessionAggregator`, `CostCalculator`, `ContextAnalyzer` |
| **Parsing** | Claude Code JSON handling | `RawStatusLine`, `RawHookEvent` |

### Panic-Free Compliance

All types in this document follow the panic-free policy:

- No `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, `todo!()`
- No direct array indexing `[i]`
- All fallible operations use `?`, `Option`, or `Result`
- Safe collection operations use `.get()`, `.first()`, `.last()`
- Arithmetic uses saturating operations where overflow is possible
