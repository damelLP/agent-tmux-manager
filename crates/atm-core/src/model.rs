//! Model identification and metadata.
//!
//! Supports known Anthropic models with pricing/context data,
//! and gracefully handles unknown models (new Anthropic releases
//! or non-Anthropic models) by preserving their raw ID for display.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Model identifier.
///
/// Parsed from status line JSON: `model.id` field.
/// Uses prefix matching for forward compatibility with new date variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Model {
    /// Claude Opus 4.6 (claude-opus-4-6-*)
    #[serde(rename = "claude-opus-4-6")]
    Opus46,

    /// Claude Opus 4.5 (claude-opus-4-5-*)
    #[serde(rename = "claude-opus-4-5-20251101")]
    Opus45,

    /// Claude Sonnet 4.5 (claude-sonnet-4-5-*)
    #[serde(rename = "claude-sonnet-4-5-20250929")]
    Sonnet45,

    /// Claude Sonnet 4 (claude-sonnet-4-*)
    #[serde(rename = "claude-sonnet-4-20250514")]
    Sonnet4,

    /// Claude Haiku 4.5 (claude-haiku-4-5-*)
    #[serde(rename = "claude-haiku-4-5-20251001")]
    Haiku45,

    /// Claude Haiku 3.5 (claude-3-5-haiku-*)
    #[serde(rename = "claude-3-5-haiku-20241022")]
    Haiku35,

    /// Claude Sonnet 3.5 v2 (claude-3-5-sonnet-*)
    #[serde(rename = "claude-3-5-sonnet-20241022")]
    Sonnet35V2,

    /// Unknown or non-Anthropic model
    #[serde(other)]
    Unknown,
}

impl Model {
    /// Returns a human-readable display name for known models.
    ///
    /// For `Unknown` models, callers should use [`derive_display_name`]
    /// with the raw model ID for a better fallback.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Opus46 => "Opus 4.6",
            Self::Opus45 => "Opus 4.5",
            Self::Sonnet45 => "Sonnet 4.5",
            Self::Sonnet4 => "Sonnet 4",
            Self::Haiku45 => "Haiku 4.5",
            Self::Haiku35 => "Haiku 3.5",
            Self::Sonnet35V2 => "Sonnet 3.5 v2",
            Self::Unknown => "Unknown",
        }
    }

    /// Returns the context window size for this model.
    pub fn context_window_size(&self) -> u32 {
        match self {
            Self::Opus46 => 200_000,
            Self::Opus45 => 200_000,
            Self::Sonnet45 => 200_000,
            Self::Sonnet4 => 200_000,
            Self::Haiku45 => 200_000,
            Self::Haiku35 => 200_000,
            Self::Sonnet35V2 => 200_000,
            Self::Unknown => 200_000, // Default assumption
        }
    }

    /// Returns approximate cost per million input tokens (USD).
    pub fn input_cost_per_million(&self) -> f64 {
        match self {
            Self::Opus46 => 5.00,
            Self::Opus45 => 15.00,
            Self::Sonnet45 => 3.00,
            Self::Sonnet4 => 3.00,
            Self::Haiku45 => 1.00,
            Self::Haiku35 => 0.80,
            Self::Sonnet35V2 => 3.00,
            Self::Unknown => 3.00, // Conservative default
        }
    }

    /// Returns approximate cost per million output tokens (USD).
    pub fn output_cost_per_million(&self) -> f64 {
        match self {
            Self::Opus46 => 25.00,
            Self::Opus45 => 75.00,
            Self::Sonnet45 => 15.00,
            Self::Sonnet4 => 15.00,
            Self::Haiku45 => 5.00,
            Self::Haiku35 => 4.00,
            Self::Sonnet35V2 => 15.00,
            Self::Unknown => 15.00, // Conservative default
        }
    }

    /// Parses a model from its string ID using prefix matching.
    ///
    /// More specific prefixes are checked first to avoid false matches
    /// (e.g., "claude-sonnet-4-5" before "claude-sonnet-4-").
    pub fn from_id(id: &str) -> Self {
        // More specific (longer) prefixes first to avoid false matches
        if id.starts_with("claude-opus-4-6") {
            Self::Opus46
        } else if id.starts_with("claude-opus-4-5") {
            Self::Opus45
        } else if id.starts_with("claude-sonnet-4-5") {
            Self::Sonnet45
        } else if id.starts_with("claude-sonnet-4") {
            Self::Sonnet4
        } else if id.starts_with("claude-haiku-4-5") {
            Self::Haiku45
        } else if id.starts_with("claude-3-5-haiku") {
            Self::Haiku35
        } else if id.starts_with("claude-3-5-sonnet") {
            Self::Sonnet35V2
        } else {
            Self::Unknown
        }
    }

    /// Returns true if this is an unknown/unrecognized model.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

/// Derives a human-readable display name from a raw model ID string.
///
/// Used as a fallback when `Model::from_id` returns `Unknown`.
/// Handles both Claude-style IDs and arbitrary model IDs:
/// - `"claude-opus-4-7-20260501"` → `"claude-opus-4-7"`
/// - `"gpt-4o"` → `"gpt-4o"`
/// - `"gemini-1.5-pro"` → `"gemini-1.5-pro"`
pub fn derive_display_name(id: &str) -> String {
    // Strip date suffix if present (pattern: -YYYYMMDD at end)
    if id.len() > 9 {
        let potential_date = &id[id.len() - 8..];
        if potential_date.chars().all(|c| c.is_ascii_digit()) {
            if let Some(base) = id[..id.len() - 8].strip_suffix('-') {
                return base.to_string();
            }
        }
    }
    id.to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Known model parsing ──

    #[test]
    fn test_model_parsing_opus45() {
        let model: Model = serde_json::from_str("\"claude-opus-4-5-20251101\"").unwrap();
        assert_eq!(model, Model::Opus45);
        assert_eq!(model.display_name(), "Opus 4.5");
    }

    #[test]
    fn test_model_parsing_opus46() {
        // Opus 4.6 may come without date suffix
        assert_eq!(Model::from_id("claude-opus-4-6"), Model::Opus46);
        assert_eq!(Model::Opus46.display_name(), "Opus 4.6");
    }

    #[test]
    fn test_model_parsing_sonnet45() {
        assert_eq!(
            Model::from_id("claude-sonnet-4-5-20250929"),
            Model::Sonnet45
        );
        assert_eq!(Model::Sonnet45.display_name(), "Sonnet 4.5");
    }

    #[test]
    fn test_model_parsing_haiku45() {
        assert_eq!(Model::from_id("claude-haiku-4-5-20251001"), Model::Haiku45);
        assert_eq!(Model::Haiku45.display_name(), "Haiku 4.5");
    }

    #[test]
    fn test_model_unknown_serde() {
        let model: Model = serde_json::from_str("\"gpt-4o\"").unwrap();
        assert_eq!(model, Model::Unknown);
    }

    // ── Prefix matching ──

    #[test]
    fn test_from_id_prefix_exact() {
        assert_eq!(Model::from_id("claude-opus-4-5-20251101"), Model::Opus45);
        assert_eq!(Model::from_id("claude-sonnet-4-20250514"), Model::Sonnet4);
    }

    #[test]
    fn test_from_id_prefix_with_different_date() {
        // Future date variants should still match the right model family
        assert_eq!(Model::from_id("claude-opus-4-6-20260301"), Model::Opus46);
        assert_eq!(
            Model::from_id("claude-sonnet-4-5-20261201"),
            Model::Sonnet45
        );
        assert_eq!(Model::from_id("claude-haiku-4-5-20260601"), Model::Haiku45);
        assert_eq!(Model::from_id("claude-opus-4-5-20260101"), Model::Opus45);
    }

    #[test]
    fn test_from_id_prefix_no_date() {
        // Model IDs without date suffix
        assert_eq!(Model::from_id("claude-opus-4-6"), Model::Opus46);
        assert_eq!(Model::from_id("claude-sonnet-4-5"), Model::Sonnet45);
    }

    #[test]
    fn test_from_id_sonnet4_not_confused_with_sonnet45() {
        // "claude-sonnet-4-5" should match Sonnet45, not Sonnet4
        assert_eq!(
            Model::from_id("claude-sonnet-4-5-20250929"),
            Model::Sonnet45
        );
        // "claude-sonnet-4-20250514" should match Sonnet4
        assert_eq!(Model::from_id("claude-sonnet-4-20250514"), Model::Sonnet4);
    }

    // ── Unknown / non-Anthropic models ──

    #[test]
    fn test_from_id_unknown() {
        assert_eq!(Model::from_id("gpt-4o"), Model::Unknown);
        assert_eq!(Model::from_id("gemini-1.5-pro"), Model::Unknown);
        assert_eq!(Model::from_id("llama-3-70b"), Model::Unknown);
        assert_eq!(Model::from_id("unknown-model"), Model::Unknown);
    }

    #[test]
    fn test_is_unknown() {
        assert!(Model::Unknown.is_unknown());
        assert!(!Model::Opus46.is_unknown());
    }

    // ── Display name derivation for unknown models ──

    #[test]
    fn test_derive_display_name_strips_date() {
        assert_eq!(
            derive_display_name("claude-opus-4-7-20260501"),
            "claude-opus-4-7"
        );
        assert_eq!(
            derive_display_name("claude-sonnet-5-20270101"),
            "claude-sonnet-5"
        );
    }

    #[test]
    fn test_derive_display_name_no_date() {
        assert_eq!(derive_display_name("gpt-4o"), "gpt-4o");
        assert_eq!(derive_display_name("gemini-1.5-pro"), "gemini-1.5-pro");
    }

    #[test]
    fn test_derive_display_name_short_ids() {
        assert_eq!(derive_display_name("gpt-4"), "gpt-4");
        assert_eq!(derive_display_name("o1"), "o1");
    }
}
