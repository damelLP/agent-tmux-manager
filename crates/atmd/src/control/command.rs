//! Pure shell command construction from resolved config and spawn overrides.

use atm_core::{Harness, LaunchPolicy};

/// Adapter-owned launch requirements for supported built-in harnesses.
#[must_use]
pub fn launch_policy(harness: Harness) -> Option<&'static LaunchPolicy> {
    const POLICIES: &[(Harness, &LaunchPolicy)] = &[
        (
            Harness::ClaudeCode,
            &atm_claude_adapter::screen::LAUNCH_POLICY,
        ),
        (Harness::Pi, &atm_pi_adapter::screen::LAUNCH_POLICY),
        (Harness::Codex, &atm_codex_adapter::screen::LAUNCH_POLICY),
    ];
    POLICIES
        .iter()
        .find_map(|(id, policy)| (*id == harness).then_some(*policy))
}

/// Resolve a spawn override using the caller's environment reader.
/// `harness_id` is canonical; its uppercase form replaces hyphens with underscores.
/// A nonempty harness-specific value wins over the global value. Whitespace is
/// preserved, and an absent or empty value falls back. BIN and ARGS are independent.
#[must_use]
pub fn spawn_env_override(
    harness_id: &str,
    suffix: &str,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    let harness_key = format!(
        "ATM_SPAWN_{}_{}",
        harness_id.replace('-', "_").to_ascii_uppercase(),
        suffix
    );
    lookup(&harness_key)
        .filter(|s| !s.is_empty())
        .or_else(|| lookup(&format!("ATM_SPAWN_{suffix}")).filter(|s| !s.is_empty()))
}

/// POSIX shell-quote `s` by single-quoting and escaping embedded `'`.
/// Result is exactly one shell token regardless of spaces or specials.
#[must_use]
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Leaves a command word bare when it is only `[A-Za-z0-9_./-]`, so the
/// shell still expands an alias of that name; quoting it would skip the
/// alias. Anything else (including empty) goes through `shell_quote`, and
/// so does a leading `-`, which `tmux send-keys` would read as its own flag.
#[must_use]
fn shell_word(s: &str) -> String {
    let plain = !s.is_empty()
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-'));
    if plain {
        s.to_string()
    } else {
        shell_quote(s)
    }
}

/// Build the command from resolved overrides, without reading environment or
/// config. Callers resolve empty-as-unset and harness/global precedence.
/// A binary override replaces ordinary defaults, never required or extra args.
/// Raw shell args (trimmed) precede quoted required and role/extra arguments;
/// the model follows them. Cwd is shell-quoted. Exact required-token matches
/// in defaults/extra args are emitted once in the required block, in first-seen
/// policy order. This is token normalization, not option/value parsing; raw
/// shell args are caller-owned and are never parsed, deduplicated or validated.
#[must_use]
#[allow(clippy::too_many_arguments)] // Resolved fields avoid duplicating a harness type.
pub fn build_spawn_command(
    binary: &str,
    default_args: &[String],
    model_flag: Option<&str>,
    cwd: Option<&str>,
    model: Option<&str>,
    bin_override: Option<&str>,
    args_override: Option<&str>,
    extra_args: &[String],
    required_args: &[&str],
) -> String {
    let base = bin_override.unwrap_or(binary);
    let mut cmd = shell_word(base);

    if bin_override.is_none() {
        for arg in default_args
            .iter()
            .filter(|arg| !required_args.contains(&arg.as_str()))
        {
            cmd.push(' ');
            cmd.push_str(&shell_quote(arg));
        }
    }

    if let Some(args) = args_override {
        cmd.push(' ');
        cmd.push_str(args.trim());
    }

    let mut seen = std::collections::HashSet::new();
    for arg in required_args
        .iter()
        .copied()
        .filter(|arg| seen.insert(*arg))
        .chain(
            extra_args
                .iter()
                .map(String::as_str)
                .filter(|arg| !required_args.contains(arg)),
        )
    {
        cmd.push(' ');
        cmd.push_str(&shell_quote(arg));
    }

    if let (Some(flag), Some(m)) = (model_flag, model) {
        cmd.push_str(&format!(" {flag} {}", shell_quote(m)));
    }
    if let Some(dir) = cwd {
        cmd = format!("cd {} && {cmd}", shell_quote(dir));
    }
    cmd
}

#[cfg(test)]
mod shell_quote_tests {
    use super::shell_word;

    /// The spawn binary is the pane shell's command word. bash and zsh
    /// expand an alias only for an unquoted word, and Claude Code's local
    /// install is reachable only as `alias claude="~/.claude/local/claude"`
    /// (2.1.310's "Local installation not accessible" fix text). So a plain
    /// word is typed bare, and anything a shell could expand or split stays
    /// quoted. A leading `-` stays quoted too: the command goes through
    /// `tmux send-keys -t <pane> <keys>` first, and when there is no `cd`
    /// prefix, tmux reads a bare `-x` as its own flag
    /// (`command send-keys: unknown flag -x`, tmux 3.8).
    #[test]
    fn shell_word_leaves_only_plain_words_bare() {
        for plain in [
            "claude",
            "pi",
            "codex",
            "/home/u/.claude/local/claude",
            "./bin/agent-1.2_x",
            "x-",
        ] {
            assert_eq!(shell_word(plain), plain, "{plain:?} should be bare");
        }

        for (word, quoted) in [
            ("my agent", "'my agent'"),
            ("it's", "'it'\\''s'"),
            ("$HOME/claude", "'$HOME/claude'"),
            ("~/.claude/local/claude", "'~/.claude/local/claude'"),
            ("=claude", "'=claude'"),
            ("FOO=1", "'FOO=1'"),
            ("", "''"),
            ("-x", "'-x'"),
            ("-", "'-'"),
            ("claude;id", "'claude;id'"),
            ("cla*de", "'cla*de'"),
            ("`id`", "'`id`'"),
            ("claud\u{e9}", "'claud\u{e9}'"),
        ] {
            assert_eq!(shell_word(word), quoted, "{word:?} should stay quoted");
        }
    }
}
