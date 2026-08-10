//! Built-in coding-agent harness registry.
//!
//! The registry is intentionally data-first: spawning, process discovery,
//! and future config overlays can all consume the same definitions instead
//! of special-casing every CLI agent in each subsystem.

use crate::Harness;

/// How ATM should supply an initial prompt to a harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    /// The harness accepts a prompt via a command-line flag.
    Flag(&'static str),
    /// The harness must be launched first, then text is injected with tmux
    /// `send-keys`.
    KeystrokeInjection,
    /// ATM does not know how to pass an initial prompt for this harness yet.
    Unsupported,
}

/// A declarative process-path matcher used by daemon discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessMatcher {
    /// Path or argv exactly equals this string.
    Exact(&'static str),
    /// Path or argv ends with this suffix.
    Suffix(&'static str),
    /// Path or argv contains this substring.
    Contains(&'static str),
}

impl ProcessMatcher {
    /// Returns true if `candidate` satisfies this matcher.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        match self {
            Self::Exact(expected) => candidate == *expected,
            Self::Suffix(suffix) => candidate.ends_with(suffix),
            Self::Contains(needle) => candidate.contains(needle),
        }
    }
}

/// Metadata for a CLI coding-agent harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessDefinition {
    /// Stable CLI/config identifier (e.g. `claude`, `pi`, `codex`).
    pub id: &'static str,
    /// Alternate CLI/config identifiers accepted by lookup.
    pub aliases: &'static [&'static str],
    /// Human-readable display name.
    pub display_name: &'static str,
    /// Session harness tag used in ATM's domain model.
    pub harness: Harness,
    /// Binary to launch for `atm spawn`.
    pub binary: &'static str,
    /// Default arguments inserted immediately after the binary.
    pub default_args: &'static [&'static str],
    /// Flag used to set the model, when supported.
    pub model_flag: Option<&'static str>,
    /// How to pass an initial prompt, when/if spawn grows that option.
    pub prompt_mode: PromptMode,
    /// Arguments used for installation/version probing.
    pub version_args: &'static [&'static str],
    /// Process path/argv matchers used by discovery.
    pub process_matchers: &'static [ProcessMatcher],
    /// Whether this harness should be detected by daemon `/proc` discovery.
    ///
    /// Keep this false until a harness has an adapter/status source, otherwise
    /// ATM may show unmanaged pending sessions with misleading badges.
    pub discovery_enabled: bool,
    /// Whether bare argv0 matches are allowed during cmdline scanning.
    ///
    /// This is safe for distinctive command names like `claude`, but unsafe for
    /// short ambiguous names like `pi` (a random program can take `pi` as data).
    /// Bare matches are never accepted from arbitrary positional arguments.
    pub allow_bare_cmdline_match: bool,
}

const CLAUDE_MATCHERS: &[ProcessMatcher] = &[
    ProcessMatcher::Exact("claude"),
    ProcessMatcher::Suffix("/claude"),
    ProcessMatcher::Contains("claude/versions/"),
];

const PI_MATCHERS: &[ProcessMatcher] = &[
    ProcessMatcher::Suffix("/bin/pi"),
    ProcessMatcher::Suffix("/pi"),
    ProcessMatcher::Contains("pi-coding-agent"),
];

const CODEX_MATCHERS: &[ProcessMatcher] = &[
    ProcessMatcher::Exact("codex"),
    ProcessMatcher::Suffix("/codex"),
    ProcessMatcher::Contains("codex-cli"),
];

const AMP_MATCHERS: &[ProcessMatcher] =
    &[ProcessMatcher::Exact("amp"), ProcessMatcher::Suffix("/amp")];

const QWEN_MATCHERS: &[ProcessMatcher] = &[
    ProcessMatcher::Exact("qwen"),
    ProcessMatcher::Suffix("/qwen"),
    ProcessMatcher::Exact("qwen-code"),
    ProcessMatcher::Suffix("/qwen-code"),
];

const GEMINI_MATCHERS: &[ProcessMatcher] = &[
    ProcessMatcher::Exact("gemini"),
    ProcessMatcher::Suffix("/gemini"),
    ProcessMatcher::Contains("gemini-cli"),
];

const DEVIN_MATCHERS: &[ProcessMatcher] = &[
    ProcessMatcher::Exact("devin"),
    ProcessMatcher::Suffix("/devin"),
    ProcessMatcher::Contains("devin/cli/"),
];

const COPILOT_MATCHERS: &[ProcessMatcher] = &[
    ProcessMatcher::Exact("copilot"),
    ProcessMatcher::Suffix("/copilot"),
];

/// Built-in harness definitions.
pub const BUILTIN_HARNESSES: &[HarnessDefinition] = &[
    HarnessDefinition {
        id: "claude",
        aliases: &["claude_code", "claude-code", "cc"],
        display_name: "Claude Code",
        harness: Harness::ClaudeCode,
        binary: "claude",
        default_args: &[],
        model_flag: Some("--model"),
        prompt_mode: PromptMode::KeystrokeInjection,
        version_args: &["--version"],
        process_matchers: CLAUDE_MATCHERS,
        discovery_enabled: true,
        allow_bare_cmdline_match: true,
    },
    HarnessDefinition {
        id: "pi",
        aliases: &[],
        display_name: "pi",
        harness: Harness::Pi,
        binary: "pi",
        default_args: &[],
        model_flag: Some("--model"),
        prompt_mode: PromptMode::KeystrokeInjection,
        version_args: &["--version"],
        process_matchers: PI_MATCHERS,
        discovery_enabled: true,
        allow_bare_cmdline_match: false,
    },
    HarnessDefinition {
        id: "codex",
        aliases: &["codex-cli"],
        display_name: "Codex CLI",
        harness: Harness::Codex,
        binary: "codex",
        default_args: &[],
        model_flag: None,
        prompt_mode: PromptMode::KeystrokeInjection,
        version_args: &["--version"],
        process_matchers: CODEX_MATCHERS,
        discovery_enabled: false,
        allow_bare_cmdline_match: true,
    },
    HarnessDefinition {
        id: "amp",
        aliases: &[],
        display_name: "Amp",
        harness: Harness::Amp,
        binary: "amp",
        default_args: &[],
        model_flag: None,
        prompt_mode: PromptMode::KeystrokeInjection,
        version_args: &["--version"],
        process_matchers: AMP_MATCHERS,
        discovery_enabled: false,
        allow_bare_cmdline_match: true,
    },
    HarnessDefinition {
        id: "qwen",
        aliases: &["qwen-code"],
        display_name: "Qwen Code",
        harness: Harness::Qwen,
        binary: "qwen",
        default_args: &[],
        model_flag: None,
        prompt_mode: PromptMode::KeystrokeInjection,
        version_args: &["--version"],
        process_matchers: QWEN_MATCHERS,
        discovery_enabled: false,
        allow_bare_cmdline_match: true,
    },
    HarnessDefinition {
        id: "gemini",
        aliases: &["gemini-cli"],
        display_name: "Gemini CLI",
        harness: Harness::Gemini,
        binary: "gemini",
        default_args: &[],
        model_flag: None,
        prompt_mode: PromptMode::KeystrokeInjection,
        version_args: &["--version"],
        process_matchers: GEMINI_MATCHERS,
        discovery_enabled: false,
        allow_bare_cmdline_match: true,
    },
    HarnessDefinition {
        id: "devin",
        aliases: &["devin-cli"],
        display_name: "Devin CLI",
        harness: Harness::Devin,
        binary: "devin",
        default_args: &[],
        model_flag: Some("--model"),
        prompt_mode: PromptMode::KeystrokeInjection,
        version_args: &["version"],
        process_matchers: DEVIN_MATCHERS,
        discovery_enabled: true,
        allow_bare_cmdline_match: true,
    },
    HarnessDefinition {
        id: "copilot",
        aliases: &["copilot-cli", "gh-copilot"],
        display_name: "GitHub Copilot CLI",
        harness: Harness::Copilot,
        binary: "copilot",
        default_args: &[],
        model_flag: Some("--model"),
        prompt_mode: PromptMode::KeystrokeInjection,
        version_args: &["--version"],
        process_matchers: COPILOT_MATCHERS,
        discovery_enabled: true,
        allow_bare_cmdline_match: true,
    },
];

/// Returns the default harness definition used by `atm spawn`.
#[must_use]
pub fn default_harness_definition() -> &'static HarnessDefinition {
    // Keep the historic default behavior: `atm spawn` launches Claude Code.
    if let Some(definition) = find_harness_definition("claude") {
        definition
    } else {
        // BUILTIN_HARNESSES is defined in this module with Claude first; this
        // defensive fallback avoids panicking if that invariant changes.
        BUILTIN_HARNESSES
            .iter()
            .next()
            .unwrap_or(&FALLBACK_HARNESS_DEFINITION)
    }
}

const FALLBACK_HARNESS_DEFINITION: HarnessDefinition = HarnessDefinition {
    id: "unknown",
    aliases: &[],
    display_name: "unknown",
    harness: Harness::Unknown,
    binary: "claude",
    default_args: &[],
    model_flag: None,
    prompt_mode: PromptMode::Unsupported,
    version_args: &[],
    process_matchers: &[],
    discovery_enabled: false,
    allow_bare_cmdline_match: false,
};

/// Finds a built-in harness definition by canonical id or alias.
#[must_use]
pub fn find_harness_definition(id: &str) -> Option<&'static HarnessDefinition> {
    BUILTIN_HARNESSES
        .iter()
        .find(|definition| definition.id == id || definition.aliases.contains(&id))
}

/// Iterates over built-in harness definitions in discovery priority order.
pub fn builtin_harnesses() -> impl Iterator<Item = &'static HarnessDefinition> {
    BUILTIN_HARNESSES.iter()
}

/// Returns a comma-separated list of built-in harness ids for diagnostics.
#[must_use]
pub fn builtin_harness_ids_display() -> String {
    BUILTIN_HARNESSES
        .iter()
        .map(|definition| definition.id)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_default_claude_harness() {
        let definition = default_harness_definition();
        assert_eq!(definition.id, "claude");
        assert_eq!(definition.harness, Harness::ClaudeCode);
    }

    #[test]
    fn aliases_resolve_to_canonical_harnesses() {
        assert_eq!(
            find_harness_definition("claude_code").map(|d| d.id),
            Some("claude")
        );
        assert_eq!(
            find_harness_definition("qwen-code").map(|d| d.id),
            Some("qwen")
        );
        assert_eq!(find_harness_definition("nope").map(|d| d.id), None);
    }

    #[test]
    fn process_matchers_cover_expected_paths() {
        let claude = find_harness_definition("claude").unwrap_or(default_harness_definition());
        assert!(claude.process_matchers.iter().any(|m| m.matches("claude")));
        assert!(claude
            .process_matchers
            .iter()
            .any(|m| m.matches("/usr/local/bin/claude")));

        let pi = find_harness_definition("pi").unwrap_or(default_harness_definition());
        assert!(pi.process_matchers.iter().any(|m| m.matches("/usr/bin/pi")));
        assert!(pi.discovery_enabled);
        assert!(!pi.allow_bare_cmdline_match);
    }
}
