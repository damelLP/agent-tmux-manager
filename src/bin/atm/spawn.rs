//! `atm spawn`: harness config and resolution, shell quoting, and the command typed into a pane.

use std::collections::HashMap;
use std::fs;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::warn;

use atm_core::{
    builtin_harnesses, default_harness_definition, find_harness_definition, HarnessDefinition,
};
use atm_tmux::{RealTmuxClient, TmuxClient};
use atm_tui::setup;
use atm_tui::tmux;

use super::SpawnDirection;
#[cfg(test)]
use super::TEST_ENV_LOCK;

/// POSIX shell-quote `s` by single-quoting and escaping embedded `'`.
/// Result is exactly one shell token regardless of spaces or specials.
pub(super) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Leaves a command word bare when it is only `[A-Za-z0-9_./-]`, so the
/// shell still expands an alias of that name; quoting it would skip the
/// alias. Anything else (including empty) goes through `shell_quote`, and
/// so does a leading `-`, which `tmux send-keys` would read as its own flag.
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

/// Builds a shell-safe `tmux` command prefix for generated hook scripts.
pub(super) fn tmux_shell_prefix(socket: Option<String>) -> String {
    match socket {
        Some(s) => format!("tmux -L {}", shell_quote(&s)),
        None => "tmux".to_string(),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AtmConfig {
    #[serde(default)]
    harness: HarnessConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct HarnessConfig {
    /// Default harness id/alias used by `atm workspace create`, and by
    /// `atm spawn` when it omits `--harness`.
    default: Option<String>,
    /// Per-harness spawn overrides from `[harness.<name>]` tables.
    #[serde(flatten)]
    definitions: HashMap<String, HarnessTomlConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct HarnessTomlConfig {
    /// Binary or wrapper command to launch.
    binary: Option<String>,
    /// Default arguments inserted after `binary` and before any model flag.
    default_args: Option<Vec<String>>,
    /// Optional model flag override. Empty string disables model support.
    model_flag: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct SpawnHarnessDefinition {
    id: String,
    binary: String,
    default_args: Vec<String>,
    model_flag: Option<String>,
}

impl SpawnHarnessDefinition {
    fn from_builtin(definition: &HarnessDefinition) -> Self {
        Self {
            id: definition.id.to_string(),
            binary: definition.binary.to_string(),
            default_args: definition
                .default_args
                .iter()
                .map(|arg| (*arg).to_string())
                .collect(),
            model_flag: definition.model_flag.map(str::to_string),
        }
    }

    fn from_custom(id: &str, config: &HarnessTomlConfig) -> Result<Self> {
        let binary = config
            .binary
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("custom harness '{id}' requires binary"))?;
        Ok(Self {
            id: id.to_string(),
            binary: binary.to_string(),
            default_args: config.default_args.clone().unwrap_or_default(),
            model_flag: config.model_flag.as_deref().and_then(non_empty_str),
        })
    }

    fn apply_config(&mut self, config: &HarnessTomlConfig) {
        if let Some(binary) = config.binary.as_deref().filter(|s| !s.is_empty()) {
            self.binary = binary.to_string();
        }
        if let Some(args) = &config.default_args {
            self.default_args.clone_from(args);
        }
        if let Some(flag) = &config.model_flag {
            self.model_flag = non_empty_str(flag);
        }
    }
}

fn non_empty_str(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn load_atm_config() -> Result<AtmConfig> {
    let Some(path) = setup::atm_config_path() else {
        return Ok(AtmConfig::default());
    };
    if !path.exists() {
        // Missing config is not an error: fall back to built-in defaults silently.
        // `atm setup` is the only writer of this file.
        return Ok(AtmConfig::default());
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("Failed to parse config {}", path.display()))
}

pub(super) fn resolve_spawn_harness(harness_id: Option<&str>) -> Result<SpawnHarnessDefinition> {
    let config = load_atm_config()?;
    let requested = harness_id
        .or(config.harness.default.as_deref())
        .unwrap_or(default_harness_definition().id);

    if let Some(builtin) = find_harness_definition(requested) {
        let mut definition = SpawnHarnessDefinition::from_builtin(builtin);
        if let Some(overlay) = config.harness.definitions.get(builtin.id) {
            definition.apply_config(overlay);
        }
        if let Some(overlay) = config.harness.definitions.get(requested) {
            definition.apply_config(overlay);
        }
        return Ok(definition);
    }

    if let Some(custom) = config.harness.definitions.get(requested) {
        return SpawnHarnessDefinition::from_custom(requested, custom);
    }

    bail!(
        "unknown harness '{requested}' (available: {})",
        available_harness_ids_display(&config)
    )
}

fn available_harness_ids_display(config: &AtmConfig) -> String {
    let mut ids: Vec<String> = builtin_harnesses()
        .map(|definition| definition.id.to_string())
        .collect();
    let mut custom_ids: Vec<String> = config.harness.definitions.keys().cloned().collect();
    custom_ids.sort();
    for id in custom_ids {
        if !ids.iter().any(|existing| existing == &id) {
            ids.push(id);
        }
    }
    ids.join(", ")
}

pub(super) fn build_spawn_command(cwd: Option<&str>, model: Option<&str>) -> String {
    let harness = resolve_spawn_harness(None).unwrap_or_else(|e| {
        warn!(error = %e, "Failed to load spawn harness config; using built-in default");
        SpawnHarnessDefinition::from_builtin(default_harness_definition())
    });
    build_spawn_command_for_harness(&harness, cwd, model)
}

fn harness_spawn_env_key(harness: &SpawnHarnessDefinition, suffix: &str) -> String {
    format!(
        "ATM_SPAWN_{}_{}",
        harness.id.replace('-', "_").to_ascii_uppercase(),
        suffix
    )
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn spawn_env_override(harness: &SpawnHarnessDefinition, suffix: &str) -> Option<String> {
    let harness_key = harness_spawn_env_key(harness, suffix);
    non_empty_env(&harness_key).or_else(|| non_empty_env(&format!("ATM_SPAWN_{suffix}")))
}

/// Build the shell command string sent to a freshly-split tmux pane.
///
/// Honors spawn override environment variables (all treat empty as unset):
///
/// - `ATM_SPAWN_<HARNESS>_BIN` / `ATM_SPAWN_<HARNESS>_ARGS`: harness-specific
///   overrides, where `<HARNESS>` is the canonical id uppercased with `-`
///   replaced by `_` (for example `ATM_SPAWN_PI_BIN=mise` and
///   `ATM_SPAWN_PI_ARGS='x pi'`). Use these when one harness needs a custom
///   launcher without changing other harnesses.
///
/// - `ATM_SPAWN_BIN` / `ATM_SPAWN_ARGS`: legacy global overrides used when the
///   harness-specific variables are unset. These intentionally override the
///   selected harness command and are mainly for tests or whole-install wrappers.
///
/// `*_BIN` is one token: a plain word is typed bare so a shell alias of that
/// name still works, and anything else is shell-quoted, so spaces/specials in
/// a path "just work."
///
/// `*_ARGS` is inserted *between* the bin and appended model flag and is
/// interpolated verbatim — the user's intent is multi-token wrapper syntax. The
/// user is responsible for quoting anything inside `*_ARGS` that needs to be
/// one token (same contract as `bash -c "..."`).
///
/// `cwd` and `model` go through `shell_quote`, so a model name like
/// `opus 4.5` or a cwd with spaces compose safely without the caller
/// thinking about it.
pub(super) fn build_spawn_command_for_harness(
    harness: &SpawnHarnessDefinition,
    cwd: Option<&str>,
    model: Option<&str>,
) -> String {
    let spawn_bin = spawn_env_override(harness, "BIN");
    let base = spawn_bin.as_deref().unwrap_or(&harness.binary);
    let mut cmd = shell_word(base);

    if spawn_bin.is_none() {
        for arg in &harness.default_args {
            cmd.push(' ');
            cmd.push_str(&shell_quote(arg));
        }
    }

    if let Some(args) = spawn_env_override(harness, "ARGS") {
        cmd.push(' ');
        cmd.push_str(args.trim());
    }

    if let (Some(flag), Some(m)) = (harness.model_flag.as_deref(), model) {
        cmd.push_str(&format!(" {flag} {}", shell_quote(m)));
    }
    if let Some(dir) = cwd {
        cmd = format!("cd {} && {cmd}", shell_quote(dir));
    }
    cmd
}

pub(super) async fn cmd_spawn(
    harness_id: Option<String>,
    model: Option<String>,
    worktree: Option<String>,
    direction: SpawnDirection,
    size: String,
    target_pane: Option<String>,
) -> Result<()> {
    let harness = resolve_spawn_harness(harness_id.as_deref())?;
    if model.is_some() && harness.model_flag.is_none() {
        bail!("harness '{}' does not support --model yet", harness.id);
    }

    if !tmux::is_in_tmux() {
        bail!("atm spawn requires running inside tmux");
    }

    let client = RealTmuxClient::new();

    // Get current pane to split from.
    // Prefer --target-pane (passed by tmux keybindings via #{pane_id}).
    // Fall back to is_active heuristic for manual CLI invocations.
    let panes = client
        .list_panes()
        .await
        .context("Failed to list tmux panes")?;
    let current_pane = target_pane
        .as_deref()
        .or_else(|| {
            panes
                .iter()
                .find(|p| p.is_active)
                .map(|p| p.pane_id.as_str())
        })
        .unwrap_or("%0");

    // Determine working directory: explicit worktree flag, or query the target pane's cwd
    let cwd = if let Some(dir) = worktree {
        Some(dir)
    } else {
        client
            .get_pane_cwd(current_pane)
            .await
            .ok()
            .flatten()
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().to_string())
            })
    };

    let agent_cmd = build_spawn_command_for_harness(&harness, cwd.as_deref(), model.as_deref());

    let pane_dir: atm_tmux::PaneDirection = direction.into();
    // Split without a command so the pane gets an interactive shell (which has
    // the user's PATH and aliases).  Then send the command via send-keys,
    // matching the approach used by `workspace create`.
    let new_pane = client
        .split_window(current_pane, &size, pane_dir, None)
        .await
        .context("Failed to split tmux pane")?;

    client
        .send_keys(&new_pane, &agent_cmd)
        .await
        .context("Failed to send command to new pane")?;
    client
        .send_keys(&new_pane, "Enter")
        .await
        .context("Failed to send Enter to new pane")?;

    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::resolve_spawn_harness;
    use crate::{Args, Command};
    use clap::Parser;

    struct IsolatedConfigHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    impl IsolatedConfigHome {
        fn new() -> Self {
            let guard = super::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
            let prev = std::env::var_os("XDG_CONFIG_HOME");
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            Self {
                _guard: guard,
                _dir: dir,
                prev,
            }
        }
    }

    impl Drop for IsolatedConfigHome {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    #[test]
    fn spawn_harness_defaults_to_config_resolution() {
        let args = Args::try_parse_from(["atm", "spawn"]).unwrap_or_else(|e| panic!("{e}"));
        match args.command {
            Some(Command::Spawn { harness, .. }) => assert!(harness.is_none()),
            other => panic!("expected spawn command, got {other:?}"),
        }
    }

    #[test]
    fn spawn_harness_parses_long_and_short_flags() {
        let args = Args::try_parse_from(["atm", "spawn", "--harness", "pi"])
            .unwrap_or_else(|e| panic!("{e}"));
        match args.command {
            Some(Command::Spawn { harness, .. }) => assert_eq!(harness.as_deref(), Some("pi")),
            other => panic!("expected spawn command, got {other:?}"),
        }

        let args =
            Args::try_parse_from(["atm", "spawn", "-H", "codex"]).unwrap_or_else(|e| panic!("{e}"));
        match args.command {
            Some(Command::Spawn { harness, .. }) => assert_eq!(harness.as_deref(), Some("codex")),
            other => panic!("expected spawn command, got {other:?}"),
        }
    }

    #[test]
    fn resolve_spawn_harness_reports_available_ids() {
        let _config_home = IsolatedConfigHome::new();
        let err = resolve_spawn_harness(Some("nope")).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("unknown harness 'nope'"));
        assert!(message.contains("claude"));
        assert!(message.contains("pi"));
    }

    #[test]
    fn load_atm_config_is_silent_when_file_absent() {
        let config_home = IsolatedConfigHome::new();
        let expected_path = super::setup::atm_config_path()
            .expect("XDG_CONFIG_HOME is set, atm_config_path should resolve");

        let config = super::load_atm_config().unwrap_or_else(|e| panic!("{e}"));

        assert!(
            !expected_path.exists(),
            "load_atm_config must not create {} as a side effect",
            expected_path.display()
        );
        assert!(
            config.harness.default.is_none(),
            "missing config file should yield default AtmConfig"
        );
        drop(config_home);
    }
}

#[cfg(test)]
mod shell_quote_tests {
    use super::{shell_word, tmux_shell_prefix};

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

    #[test]
    fn tmux_shell_prefix_quotes_socket_label() {
        assert_eq!(tmux_shell_prefix(None), "tmux");
        assert_eq!(
            tmux_shell_prefix(Some("sock with spaces".to_string())),
            "tmux -L 'sock with spaces'"
        );
        assert_eq!(
            tmux_shell_prefix(Some("sock'; touch /tmp/pwned #".to_string())),
            "tmux -L 'sock'\\''; touch /tmp/pwned #'"
        );
    }
}

#[cfg(test)]
mod spawn_command_tests {
    use super::{
        build_spawn_command, build_spawn_command_for_harness, resolve_spawn_harness,
        SpawnHarnessDefinition,
    };
    use std::fs;

    /// RAII guard that captures an env var's current value on construction
    /// and restores it on drop (including during unwinding from a failed
    /// assertion). Without this, a mid-test panic would leak the
    /// mutated value into later tests in the same binary.
    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                prev: std::env::var_os(key),
            }
        }
        fn set(&self, value: &str) {
            std::env::set_var(self.key, value);
        }
        fn unset(&self) {
            std::env::remove_var(self.key);
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// One single `#[test]`, because all the cases mutate process-global
    /// spawn env vars, and Rust's default test runner runs tests in parallel.
    /// Splitting would race.
    /// (`serial_test` would solve that but isn't worth a dep just here.)
    #[test]
    fn build_spawn_command_cases() {
        let _guard = super::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bin = EnvGuard::capture("ATM_SPAWN_BIN");
        let args = EnvGuard::capture("ATM_SPAWN_ARGS");
        let pi_bin = EnvGuard::capture("ATM_SPAWN_PI_BIN");
        let pi_args = EnvGuard::capture("ATM_SPAWN_PI_ARGS");
        let config_home = EnvGuard::capture("XDG_CONFIG_HOME");
        let config_dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));

        // Start clean — captured env vars will be restored on drop regardless
        // of which scenarios run/panic below.
        bin.unset();
        args.unset();
        pi_bin.unset();
        pi_args.unset();
        std::env::set_var("XDG_CONFIG_HOME", config_dir.path());

        // 1. Unset env/config → default to bare `claude`. A plain word is
        //    typed unquoted so the pane's shell still expands an alias of
        //    that name; `'claude'` would skip it.
        //    spawn must NOT materialize the user's config.toml — that's
        //    the job of `atm setup`. (beads agent-tmux-manager-6fb)
        assert_eq!(build_spawn_command(None, None), "claude");
        assert!(
            !config_dir.path().join("atm/config.toml").exists(),
            "spawn must not create atm/config.toml as a side effect"
        );

        // 2. Empty value is treated as unset.
        bin.set("");
        assert_eq!(build_spawn_command(None, None), "claude");

        // 3. Path containing a space is single-quoted, so the pane's
        //    shell sees one token instead of re-tokenizing on space.
        bin.set("/Applications/Claude Code.app/Contents/MacOS/claude");
        assert_eq!(
            build_spawn_command(None, None),
            "'/Applications/Claude Code.app/Contents/MacOS/claude'"
        );

        // 4. Embedded single-quote is escaped via the standard POSIX
        //    trick: close-quote, escape literal, reopen.
        bin.set("/path/it's/claude");
        assert_eq!(build_spawn_command(None, None), "'/path/it'\\''s/claude'");

        // 5. Cwd and model compose around the command.
        bin.unset();
        assert_eq!(
            build_spawn_command(Some("/work/dir"), Some("opus")),
            "cd '/work/dir' && claude --model 'opus'"
        );

        // 6. Model containing a space is quoted, so the pane's shell
        //    treats it as a single argument value to `--model`.
        assert_eq!(
            build_spawn_command(None, Some("opus 4.5")),
            "claude --model 'opus 4.5'"
        );

        // 7. Cwd with a single quote in it composes safely too.
        assert_eq!(
            build_spawn_command(Some("/work/it's"), None),
            "cd '/work/it'\\''s' && claude"
        );

        // ----- ATM_SPAWN_ARGS -----

        // 8. ATM_SPAWN_ARGS interpolates verbatim between bin and any
        //    appended `--model`. Wrapper-command UX:
        //    `mise x claude --model opus`.
        bin.set("mise");
        args.set("x claude");
        assert_eq!(
            build_spawn_command(None, Some("opus")),
            "mise x claude --model 'opus'"
        );

        // 9. `env FOO=1 claude` style.
        bin.set("env");
        args.set("FOO=1 claude");
        assert_eq!(build_spawn_command(None, None), "env FOO=1 claude");

        // 10. Empty ATM_SPAWN_ARGS is treated as unset (no extra
        //     whitespace inserted).
        bin.unset();
        args.set("");
        assert_eq!(build_spawn_command(None, None), "claude");

        // 11. Surrounding whitespace in ATM_SPAWN_ARGS is trimmed so
        //     copy-paste mistakes don't produce stray spaces.
        bin.set("mise");
        args.set("  x claude  ");
        assert_eq!(build_spawn_command(None, None), "mise x claude");

        // 12. With cwd, args sit between bin and model as expected.
        bin.set("direnv");
        args.set("exec . claude");
        assert_eq!(
            build_spawn_command(Some("/work/dir"), Some("opus")),
            "cd '/work/dir' && direnv exec . claude --model 'opus'"
        );

        // 13. Explicit harness selection changes the launched binary while
        //     retaining model/cwd composition.
        bin.unset();
        args.unset();
        let pi = resolve_spawn_harness(Some("pi")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            build_spawn_command_for_harness(&pi, Some("/work/dir"), Some("gpt-5.5")),
            "cd '/work/dir' && pi --model 'gpt-5.5'"
        );

        let codex = resolve_spawn_harness(Some("codex")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(build_spawn_command_for_harness(&codex, None, None), "codex");

        // 14. Harness-specific overrides take precedence over the legacy
        //     global overrides, so users can customize one harness without
        //     changing every `atm spawn --harness ...` invocation.
        bin.set("global-bin");
        args.set("global args");
        pi_bin.set("mise");
        pi_args.set("x pi");
        assert_eq!(
            build_spawn_command_for_harness(&pi, None, Some("gpt-5.5")),
            "mise x pi --model 'gpt-5.5'"
        );

        // 15. Without a harness-specific override, legacy globals still apply
        //     as an intentional whole-command override.
        pi_bin.unset();
        pi_args.unset();
        assert_eq!(
            build_spawn_command_for_harness(&pi, None, None),
            "global-bin global args"
        );

        // 16. Config can set the default harness and per-harness spawn defaults.
        bin.unset();
        args.unset();
        let atm_config_dir = config_dir.path().join("atm");
        fs::create_dir_all(&atm_config_dir).unwrap_or_else(|e| panic!("{e}"));
        fs::write(
            atm_config_dir.join("config.toml"),
            "[harness]\ndefault = 'pi'\n\n[harness.pi]\nbinary = 'mise'\ndefault_args = ['x', 'pi']\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            build_spawn_command(None, Some("gpt-5.5")),
            "mise 'x' 'pi' --model 'gpt-5.5'"
        );

        // 17. Config can define a custom harness.
        fs::write(
            atm_config_dir.join("config.toml"),
            "[harness.custom]\nbinary = 'custom-agent'\ndefault_args = ['--profile', 'atm']\nmodel_flag = '--model-id'\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let custom = resolve_spawn_harness(Some("custom")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            build_spawn_command_for_harness(&custom, None, Some("abc")),
            "custom-agent '--profile' 'atm' --model-id 'abc'"
        );

        // 18. Only the command word may go bare. Default args, the model and
        //     the cwd are still quoted, even when they are plain words too.
        let plain_word_harness = SpawnHarnessDefinition {
            id: "word-probe".to_string(),
            binary: "claude".to_string(),
            default_args: vec!["--profile".to_string(), "a b".to_string()],
            model_flag: Some("--model".to_string()),
        };
        assert_eq!(
            build_spawn_command_for_harness(&plain_word_harness, Some("/work dir"), Some("opus")),
            "cd '/work dir' && claude '--profile' 'a b' --model 'opus'"
        );

        // env's Drop restores the captured values, even if any of the
        // assert_eq!s above panicked.
        let _ = config_home;
        let _ = config_dir;
    }
}
