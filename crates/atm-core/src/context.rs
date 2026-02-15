//! Context window and token tracking.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::{Add, AddAssign};

/// Represents a count of tokens.
///
/// Used for input tokens, output tokens, cache tokens.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
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

impl Add for TokenCount {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

impl AddAssign for TokenCount {
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

/// Context window usage information.
///
/// Tracks token counts and calculates usage percentage based on `current_usage`
/// from Claude Code's status line. The `current_usage` object reflects the actual
/// tokens being sent to the API in the current context window.
///
/// When `current_usage` is null (e.g., during /clear), all current_* fields are 0,
/// which correctly shows 0% context usage.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ContextUsage {
    /// Total input tokens across all turns (cumulative, for reference)
    pub total_input_tokens: TokenCount,

    /// Total output tokens across all turns (cumulative, for reference)
    pub total_output_tokens: TokenCount,

    /// Maximum context window size for the model
    pub context_window_size: u32,

    /// Current context's input tokens (from current_usage.input_tokens)
    pub current_input_tokens: TokenCount,

    /// Current context's output tokens (from current_usage.output_tokens)
    pub current_output_tokens: TokenCount,

    /// Tokens written to cache (from current_usage.cache_creation_input_tokens)
    pub cache_creation_tokens: TokenCount,

    /// Tokens read from cache - this is the bulk of context (from current_usage.cache_read_input_tokens)
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

    /// Calculates the context tokens currently in use.
    ///
    /// This is the actual context being sent to the API, calculated from:
    /// - cache_read_tokens: Previously cached context being reused
    /// - current_input_tokens: New input tokens in this turn
    /// - cache_creation_tokens: New tokens being written to cache
    ///
    /// When current_usage is null (e.g., after /clear), these are all 0.
    pub fn context_tokens(&self) -> TokenCount {
        self.cache_read_tokens
            .saturating_add(self.current_input_tokens)
            .saturating_add(self.cache_creation_tokens)
    }

    /// Calculates the total tokens used (cumulative, for reference).
    pub fn total_tokens(&self) -> TokenCount {
        self.total_input_tokens
            .saturating_add(self.total_output_tokens)
    }

    /// Returns the percentage of context window used (0.0 to 100.0).
    ///
    /// Uses context_tokens() which reflects the actual tokens in the current
    /// context window (from current_usage). When current_usage is null,
    /// this returns 0%.
    pub fn usage_percentage(&self) -> f64 {
        if self.context_window_size == 0 {
            return 0.0;
        }
        let usage = self.context_tokens().as_u64() as f64 / self.context_window_size as f64;
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
        self.context_tokens().as_u64() > 200_000
    }

    /// Returns the remaining tokens before hitting context limit.
    pub fn remaining_tokens(&self) -> TokenCount {
        let used = self.context_tokens().as_u64();
        let limit = self.context_window_size as u64;
        TokenCount::new(limit.saturating_sub(used))
    }

    /// Formats usage for display (e.g., "45.2% (26.4K/200K)").
    pub fn format(&self) -> String {
        format!(
            "{:.1}% ({}/{})",
            self.usage_percentage(),
            self.context_tokens().format(),
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

/// Service for analyzing context usage and generating warnings.
pub struct ContextAnalyzer;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_count_formatting() {
        assert_eq!(TokenCount::new(500).format(), "500");
        assert_eq!(TokenCount::new(5_000).format(), "5.0K");
        assert_eq!(TokenCount::new(50_000).format(), "50K");
        assert_eq!(TokenCount::new(1_500_000).format(), "1.5M");
    }

    #[test]
    fn test_usage_percentage_from_current_usage() {
        // Context tokens from current_usage: cache_read + input + cache_creation
        // 26000 cache_read + 9 input + 31 cache_creation = 26040 context tokens
        // 26040 / 200000 = 13.02%
        let usage = ContextUsage {
            cache_read_tokens: TokenCount::new(26_000),
            current_input_tokens: TokenCount::new(9),
            cache_creation_tokens: TokenCount::new(31),
            context_window_size: 200_000,
            ..Default::default()
        };
        assert!((usage.usage_percentage() - 13.02).abs() < 0.01);
        assert_eq!(usage.context_tokens().as_u64(), 26_040);
    }

    #[test]
    fn test_usage_percentage_zero_when_current_usage_null() {
        // When current_usage is null, all current_* fields are 0
        // This correctly shows 0% after /clear
        let usage = ContextUsage {
            total_input_tokens: TokenCount::new(10_000), // cumulative still present
            total_output_tokens: TokenCount::new(1_000),
            context_window_size: 200_000,
            // current_* fields all default to 0
            ..Default::default()
        };
        assert!((usage.usage_percentage() - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_warning_thresholds() {
        // 50% usage from cache_read
        let normal = ContextUsage {
            cache_read_tokens: TokenCount::new(100_000),
            context_window_size: 200_000,
            ..Default::default()
        };
        assert!(!normal.is_warning());
        assert!(!normal.is_critical());
        assert_eq!(
            ContextAnalyzer::analyze(&normal),
            ContextWarningLevel::Normal
        );

        // 80% usage
        let warning = ContextUsage {
            cache_read_tokens: TokenCount::new(160_000),
            context_window_size: 200_000,
            ..Default::default()
        };
        assert!(warning.is_warning());
        assert!(!warning.is_critical());
        assert_eq!(
            ContextAnalyzer::analyze(&warning),
            ContextWarningLevel::Warning
        );

        // 95% usage
        let critical = ContextUsage {
            cache_read_tokens: TokenCount::new(190_000),
            context_window_size: 200_000,
            ..Default::default()
        };
        assert!(critical.is_warning());
        assert!(critical.is_critical());
        assert_eq!(
            ContextAnalyzer::analyze(&critical),
            ContextWarningLevel::Critical
        );
    }

    #[test]
    fn test_remaining_tokens() {
        // 100K context tokens = 100K remaining
        let usage = ContextUsage {
            cache_read_tokens: TokenCount::new(100_000),
            context_window_size: 200_000,
            ..Default::default()
        };
        assert_eq!(usage.remaining_tokens().as_u64(), 100_000);
    }

    #[test]
    fn test_context_tokens_calculation() {
        let usage = ContextUsage {
            cache_read_tokens: TokenCount::new(25_000),
            current_input_tokens: TokenCount::new(500),
            cache_creation_tokens: TokenCount::new(100),
            context_window_size: 200_000,
            ..Default::default()
        };
        // 25000 + 500 + 100 = 25600
        assert_eq!(usage.context_tokens().as_u64(), 25_600);
    }
}
