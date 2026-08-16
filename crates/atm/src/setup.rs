//! Setup for ATM integrations across coding-agent harnesses.
//!
//! Detects which harnesses are installed (Claude Code, pi, Codex CLI,
//! future) and wires the matching hook for each:
//!
//! - **Claude Code**: writes the `atm-hook` bash script to
//!   `~/.local/bin/`, then registers it in `~/.claude/settings.json`'s
//!   `hooks` and `statusLine` blocks.
//! - **pi** (<https://pi.dev/>): writes the `pi-atm` TypeScript
//!   extension to `~/.pi/agent/packages/pi-atm/`, then adds
//!   `"packages/pi-atm"` to `~/.pi/agent/settings.json`'s `packages`
//!   array (the entry is resolved relative to pi's `agentDir`).
//!   Mirrors how pi-amplike documents local-dev installs.
//! - **Codex CLI**: writes the `atm-codex-hook` bash script to
//!   `~/.local/bin/`, then registers it for every Codex hook event in
//!   `~/.codex/hooks.json`'s `hooks` object (5s timeout per hook; no
//!   statusLine equivalent exists). Codex requires a one-time trust
//!   approval of non-managed hooks — `setup_codex` prints the `/hooks`
//!   instruction for this.

use std::fs;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// The atm-hook bash script content (Claude Code), embedded at compile time.
const ATM_HOOK_SCRIPT: &str = include_str!("../scripts/atm-hook");

/// The atm-codex-hook bash script content (Codex CLI), embedded at compile time.
const ATM_CODEX_HOOK_SCRIPT: &str = include_str!("../scripts/atm-codex-hook");

/// The pi-atm TypeScript extension content, embedded at compile time.
/// pi loads `.ts` files directly via `@mariozechner/jiti`.
const PI_ATM_EXTENSION: &str = include_str!("../assets/pi-atm/extensions/pi-atm.ts");

/// Package manifest written next to the embedded extension. Pi looks
/// at the `pi.extensions` array (not `main`) to discover extension
/// files within an installed package.
const PI_ATM_PACKAGE_JSON: &str = include_str!("../assets/pi-atm/package.json");

/// All valid Claude Code hook types.
/// See: https://docs.anthropic.com/en/docs/claude-code/hooks
const HOOK_TYPES: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "UserPromptSubmit",
    "SessionStart",
    "SessionEnd",
    "Stop",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PermissionRequest",
];

/// All Codex CLI hook types, per the official hooks documentation
/// (validated against codex-cli 0.146.1). Unlike Claude, Codex has no
/// `PostToolUseFailure`/`Setup`/`Notification`; it adds
/// `PermissionRequest` and `PostCompact`.
const CODEX_HOOK_TYPES: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "UserPromptSubmit",
    "SessionStart",
    "SessionEnd",
    "Stop",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
];

/// Returns the path to Claude Code settings.json
fn claude_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// Returns the path to Codex's hooks.json
fn codex_hooks_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("hooks.json"))
}

/// Returns the path to the atm-codex-hook script
fn codex_hook_script_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".local").join("bin").join("atm-codex-hook"))
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/atm-codex-hook"))
}

/// Returns the path to the atm-hook script
fn hook_script_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".local").join("bin").join("atm-hook"))
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/atm-hook"))
}

/// Reads a JSON file at `path`, returning an empty object if the file
/// does not exist. Errors carry the path for diagnostics.
fn read_json_file_or_empty(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;

    serde_json::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))
}

/// Writes `value` to `path` as pretty-printed JSON, creating parent
/// directories as needed. Errors carry the path for diagnostics.
fn write_json_file_pretty(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let content = serde_json::to_string_pretty(value)?;
    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))
}

/// Reads Claude Code settings, returns empty object if file doesn't exist
fn read_settings() -> Result<Value> {
    let path = claude_settings_path().context("Could not determine home directory")?;
    read_json_file_or_empty(&path)
}

/// Writes Claude Code settings
fn write_settings(settings: &Value) -> Result<()> {
    let path = claude_settings_path().context("Could not determine home directory")?;
    write_json_file_pretty(&path, settings)
}

/// Creates the statusLine configuration entry.
///
/// Uses the same atm-hook script which auto-detects message type.
fn create_status_line_entry() -> Value {
    let hook_path = hook_script_path();
    let command = hook_path.to_string_lossy().to_string();

    json!({
        "type": "command",
        "command": command
    })
}

/// Checks if atm-hook is configured for statusLine
fn has_atm_status_line(status_line: &Value) -> bool {
    status_line
        .get("command")
        .and_then(|c| c.as_str())
        .map(|cmd| cmd.contains("atm-hook"))
        .unwrap_or(false)
}

/// Creates a hook entry for the given hook type.
///
/// Hook types that filter by tool name use a matcher, others don't.
fn create_hook_entry(hook_type: &str) -> Value {
    let hook_path = hook_script_path();
    let command = hook_path.to_string_lossy().to_string();

    // Tool-related hooks use a matcher to filter by tool name
    let needs_matcher = matches!(
        hook_type,
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "PermissionRequest"
    );

    if needs_matcher {
        json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": command
            }]
        })
    } else {
        // Session/lifecycle hooks don't use a matcher
        json!({
            "hooks": [{
                "type": "command",
                "command": command
            }]
        })
    }
}

/// Checks if a hook entry whose command contains `marker` is present
/// in a hooks array. `marker` is the vendor script's basename
/// (`atm-hook`, `atm-codex-hook`); the two never substring-match each
/// other, and each lives in a different vendor settings file anyway.
fn has_hook_command_marker(hooks_array: &[Value], marker: &str) -> bool {
    hooks_array.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(|cmd| cmd.contains(marker))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    })
}

/// Removes entries whose command contains `marker` from a hooks array.
fn remove_hook_command_marker(hooks_array: &mut Vec<Value>, marker: &str) {
    hooks_array.retain(|entry| {
        !entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(|cmd| cmd.contains(marker))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });
}

/// Writes `content` to `path` (creating parent directories as needed)
/// and marks it executable.
fn install_executable_script(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)
            .with_context(|| format!("Failed to set permissions on {}", path.display()))?;
    }

    Ok(())
}

/// Removes the script at `path`, reporting whether it existed.
fn remove_script_file(path: &Path) -> Result<bool> {
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("Failed to remove {}", path.display()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Installs the ATM tmux keybindings file to ~/.config/atm/tmux-bindings.conf.
fn install_tmux_bindings() -> Result<()> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine config directory"))?
        .join("atm");

    std::fs::create_dir_all(&config_dir)?;

    let bindings_path = config_dir.join("tmux-bindings.conf");
    let content = r#"# ATM — Agent Tmux Manager bindings
# Source this in your .tmux.conf: source-file ~/.config/atm/tmux-bindings.conf

# Spawn a new Claude agent (default: below current pane)
bind C-n run-shell "atm spawn --target-pane #{pane_id}"

# Directional agent spawn (vim-style: h=left, j=below, k=above, l=right)
bind C-h run-shell "atm spawn --direction left --target-pane #{pane_id}"
bind C-j run-shell "atm spawn --direction below --target-pane #{pane_id}"
bind C-k run-shell "atm spawn --direction above --target-pane #{pane_id}"
bind C-l run-shell "atm spawn --direction right --target-pane #{pane_id}"

# Toggle ATM sidebar panel
bind C-a run-shell "atm toggle-panel"

# ATM popup overlay (alternative to sidebar)
bind C-s display-popup -E -w 35% -h 100% -x 0 "atm"

# Status bar integration (uncomment and add to status-right):
# set -g status-right '#(atm status) | %H:%M'
"#;

    std::fs::write(&bindings_path, content)?;
    println!("Installed tmux bindings: {}", bindings_path.display());
    println!(
        "Add to your .tmux.conf: source-file {}",
        bindings_path.display()
    );
    Ok(())
}

// ============================================================================
// Harness detection
// ============================================================================

/// True if Claude Code appears to be installed for this user.
///
/// We treat the existence of `~/.claude/` as authoritative — Claude
/// creates this directory on first run regardless of where its
/// binary lives.
fn detect_claude_code() -> bool {
    dirs::home_dir()
        .map(|h| h.join(".claude").exists())
        .unwrap_or(false)
}

/// True if pi appears to be installed for this user.
///
/// pi creates `~/.pi/agent/` on first run. Checking the directory
/// avoids depending on a particular install location for the binary
/// (npm global / nvm version / etc).
fn detect_pi() -> bool {
    dirs::home_dir()
        .map(|h| h.join(".pi/agent").exists())
        .unwrap_or(false)
}

/// True if the Codex CLI appears to be installed for this user.
///
/// Codex creates `~/.codex/` on first run (auth.json, config.toml,
/// sessions/), regardless of where its binary lives.
fn detect_codex() -> bool {
    dirs::home_dir()
        .map(|h| h.join(".codex").exists())
        .unwrap_or(false)
}

// ============================================================================
// Codex setup
// ============================================================================

/// Reads Codex's hooks.json. Returns an empty object if not present.
fn read_codex_hooks() -> Result<Value> {
    let path = codex_hooks_path().context("Could not determine home directory")?;
    read_json_file_or_empty(&path)
}

/// Writes Codex's hooks.json
fn write_codex_hooks(hooks: &Value) -> Result<()> {
    let path = codex_hooks_path().context("Could not determine home directory")?;
    write_json_file_pretty(&path, hooks)
}

/// Creates a Codex hook entry for the given hook type.
///
/// Shape validated against codex-cli 0.146.1: entries live under a
/// top-level `hooks` object keyed by event name; omitting `matcher`
/// fires for all tools. Codex clamps the SessionEnd hook timeout to
/// 3s; 5s is a comfortable ceiling for the fire-and-forget script
/// everywhere else.
fn create_codex_hook_entry() -> Value {
    let hook_path = codex_hook_script_path();
    let command = hook_path.to_string_lossy().to_string();

    json!({
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": 5
        }]
    })
}

// ============================================================================
// pi setup
// ============================================================================

/// Path under which we install the embedded `pi-atm` extension.
///
/// Pi resolves the `"packages/<name>"` settings entry relative to its
/// `agentDir` (`~/.pi/agent/`, not `~/.pi/`) — verified against
/// `package-manager.js`'s `globalBaseDir = this.agentDir` and
/// `agentDir = ~/.pi/agent`. So the install path is
/// `~/.pi/agent/packages/<name>/`.
fn pi_atm_package_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".pi/agent/packages/pi-atm"))
}

/// Path to pi's settings file.
fn pi_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".pi").join("agent").join("settings.json"))
}

/// Reads pi's settings.json. Returns an empty object if not present.
fn read_pi_settings() -> Result<Value> {
    let path = pi_settings_path().context("Could not determine home directory")?;
    read_json_file_or_empty(&path)
}

fn write_pi_settings(settings: &Value) -> Result<()> {
    let path = pi_settings_path().context("Could not determine home directory")?;
    write_json_file_pretty(&path, settings)
}

/// Writes the embedded pi-atm extension files to
/// `~/.pi/agent/packages/pi-atm/` (overwriting if present), then
/// ensures `"packages/pi-atm"` is in pi's settings `packages` array.
///
/// Returns `(files_written, settings_changed)` for caller's success
/// message.
fn install_pi_extension() -> Result<(bool, bool)> {
    let pkg_dir = pi_atm_package_dir().context("Could not determine home directory")?;
    let extensions_dir = pkg_dir.join("extensions");
    fs::create_dir_all(&extensions_dir)
        .with_context(|| format!("Failed to create {}", extensions_dir.display()))?;

    // Layout matches pi-amplike: package.json with pi.extensions
    // pointing at ./extensions/, and the .ts file inside.
    let ts_path = extensions_dir.join("pi-atm.ts");
    let pkg_path = pkg_dir.join("package.json");

    // Always write to refresh any in-place edits the user might have made.
    fs::write(&ts_path, PI_ATM_EXTENSION)
        .with_context(|| format!("Failed to write {}", ts_path.display()))?;
    fs::write(&pkg_path, PI_ATM_PACKAGE_JSON)
        .with_context(|| format!("Failed to write {}", pkg_path.display()))?;

    // Update pi's settings.json packages array.
    //
    // `Value` indexing panics when the inner value isn't an object —
    // a corrupted or hand-edited settings.json that's e.g. `null` or
    // `["array"]` would crash atm setup. Replace any non-object root
    // with `{}` so the indexing below is always valid; we still error
    // out below if `packages` exists but is the wrong shape.
    let mut settings = read_pi_settings()?;
    if !settings.is_object() {
        settings = json!({});
    }
    if settings.get("packages").is_none() {
        settings["packages"] = json!([]);
    }
    let packages = settings["packages"]
        .as_array_mut()
        .context("packages is not an array in pi settings.json")?;

    // Pi's local-package format: "packages/<name>" (relative to
    // pi's `agentDir`, which is `~/.pi/agent/`).
    let entry = Value::String("packages/pi-atm".to_string());
    let already_present = packages.iter().any(|v| v == &entry);
    if !already_present {
        packages.push(entry);
        write_pi_settings(&settings)?;
        Ok((true, true))
    } else {
        Ok((true, false))
    }
}

/// Removes the pi-atm extension from `~/.pi/agent/packages/pi-atm/`
/// and from pi's settings.json.
fn uninstall_pi_extension() -> Result<bool> {
    let mut changed = false;
    if let Some(pkg_dir) = pi_atm_package_dir() {
        if pkg_dir.exists() {
            fs::remove_dir_all(&pkg_dir)
                .with_context(|| format!("Failed to remove {}", pkg_dir.display()))?;
            changed = true;
        }
    }
    let mut settings = read_pi_settings()?;
    if let Some(packages) = settings.get_mut("packages").and_then(|p| p.as_array_mut()) {
        let before = packages.len();
        packages.retain(|v| v.as_str() != Some("packages/pi-atm"));
        if packages.len() < before {
            write_pi_settings(&settings)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Returns the path to atm's own configuration file (`$XDG_CONFIG_HOME/atm/config.toml`).
pub fn atm_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("atm/config.toml"))
}

fn default_atm_config_text() -> &'static str {
    r#"# ATM configuration
#
# The default harness is used when `atm spawn` omits `--harness`.
[harness]
default = "claude"

# Per-harness defaults override built-ins when no env override is set.
# Environment precedence: ATM_SPAWN_<HARNESS>_BIN/ARGS, then
# ATM_SPAWN_BIN/ARGS, then this file, then built-in defaults.
#
# Example: make `atm spawn` launch pi through mise:
# [harness]
# default = "pi"
#
# [harness.pi]
# binary = "mise"
# default_args = ["x", "pi"]
#
# Example: define a custom harness:
# [harness.custom]
# binary = "custom-agent"
# default_args = ["--profile", "atm"]
# model_flag = "--model-id"
"#
}

/// Writes the default atm config to `path` if it does not already exist.
///
/// Idempotent: an existing file is left untouched so user edits are preserved.
/// Reports back whether the file was newly created.
pub fn ensure_default_atm_config(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config directory {}", parent.display()))?;
    }
    match fs::write(path, default_atm_config_text()) {
        Ok(()) => Ok(true),
        // Lost a race with another writer — treat as already-present.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => {
            Err(e).with_context(|| format!("Failed to write default config {}", path.display()))
        }
    }
}

/// Installs atm integration for every detected coding-agent harness.
pub fn setup() -> Result<()> {
    println!("Setting up ATM...\n");

    // Detect installed harnesses up-front so the user sees what
    // will (and won't) be configured.
    let claude = detect_claude_code();
    let pi = detect_pi();
    let codex = detect_codex();

    println!("Detected coding agents:");
    println!(
        "  {} Claude Code  (~/.claude/{})",
        if claude { "✓" } else { "✗" },
        if claude { "" } else { " not present" }
    );
    println!(
        "  {} pi           (~/.pi/agent/{})",
        if pi { "✓" } else { "✗" },
        if pi { "" } else { " not present" }
    );
    println!(
        "  {} Codex CLI    (~/.codex/{})",
        if codex { "✓" } else { "✗" },
        if codex { "" } else { " not present" }
    );

    if !claude && !pi && !codex {
        println!(
            "\nNo supported agent installations found. Install Claude Code, pi, or Codex first."
        );
        return Ok(());
    }

    if claude {
        setup_claude_code()?;
    }

    if pi {
        setup_pi()?;
    }

    if codex {
        setup_codex()?;
    }

    // Step N: Install tmux keybindings (vendor-neutral).
    println!();
    install_tmux_bindings()?;

    // Step N+1: Materialize atm's own config.toml so users have a documented
    // starting point. Spawn no longer auto-creates this; setup is the only
    // writer.
    if let Some(path) = atm_config_path() {
        print!("\nWriting atm config to {}... ", path.display());
        match ensure_default_atm_config(&path)? {
            true => println!("created"),
            false => println!("already present (preserved)"),
        }
    }

    println!("\nNext step:");
    println!("  Run: atm");

    Ok(())
}

/// Wires `atm-hook` into Claude Code's `~/.claude/settings.json`.
fn setup_claude_code() -> Result<()> {
    println!("\nConfiguring Claude Code...");
    let hook_path = hook_script_path();
    print!("  Installing hook script to {}... ", hook_path.display());
    install_executable_script(&hook_path, ATM_HOOK_SCRIPT)?;
    println!("done");

    let mut settings = read_settings()?;

    // Ensure hooks object exists
    if settings.get("hooks").is_none() {
        settings["hooks"] = json!({});
    }

    let hooks = settings["hooks"]
        .as_object_mut()
        .context("hooks is not an object")?;

    let mut added = 0;

    for &hook_type in HOOK_TYPES {
        let hooks_array = hooks
            .entry(hook_type)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("hook type is not an array")?;

        if has_hook_command_marker(hooks_array, "atm-hook") {
            println!("    {hook_type} - already configured");
        } else {
            hooks_array.push(create_hook_entry(hook_type));
            added += 1;
            println!("    {hook_type} - added");
        }
    }

    // statusLine
    let status_line_configured = if let Some(existing) = settings.get("statusLine") {
        if has_atm_status_line(existing) {
            println!("    statusLine - already configured");
            false
        } else {
            settings["statusLine"] = create_status_line_entry();
            println!("    statusLine - updated to use atm-hook");
            true
        }
    } else {
        settings["statusLine"] = create_status_line_entry();
        println!("    statusLine - added");
        true
    };

    if added > 0 || status_line_configured {
        write_settings(&settings)?;
        println!("  Claude Code configuration written.");
    } else {
        println!("  Claude Code already configured.");
    }
    Ok(())
}

/// Installs the `pi-atm` extension into `~/.pi/agent/packages/pi-atm/`
/// and registers it in pi's settings.json `packages` array.
fn setup_pi() -> Result<()> {
    println!("\nConfiguring pi...");
    let (files_written, settings_changed) = install_pi_extension()?;
    if files_written {
        let pkg_dir = pi_atm_package_dir().unwrap_or_default();
        println!("    pi-atm.ts written to {}", pkg_dir.display());
    }
    if settings_changed {
        println!("    settings.json - added 'packages/pi-atm'");
    } else {
        println!("    settings.json - already references 'packages/pi-atm'");
    }
    println!("  pi configuration written.");
    Ok(())
}

/// Wires `atm-codex-hook` into Codex's `~/.codex/hooks.json`.
fn setup_codex() -> Result<()> {
    println!("\nConfiguring Codex CLI...");
    let hook_path = codex_hook_script_path();
    print!("  Installing hook script to {}... ", hook_path.display());
    install_executable_script(&hook_path, ATM_CODEX_HOOK_SCRIPT)?;
    println!("done");

    // A corrupted or hand-edited hooks.json that isn't an object would
    // make the indexing below panic; replace any non-object root with
    // `{}` (mirrors the guard in install_pi_extension).
    let mut settings = read_codex_hooks()?;
    if !settings.is_object() {
        settings = json!({});
    }

    if settings.get("hooks").is_none() {
        settings["hooks"] = json!({});
    }

    let hooks = settings["hooks"]
        .as_object_mut()
        .context("hooks is not an object in codex hooks.json")?;

    let mut added = 0;

    for &hook_type in CODEX_HOOK_TYPES {
        let hooks_array = hooks
            .entry(hook_type)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("hook type is not an array")?;

        if has_hook_command_marker(hooks_array, "atm-codex-hook") {
            println!("    {hook_type} - already configured");
        } else {
            hooks_array.push(create_codex_hook_entry());
            added += 1;
            println!("    {hook_type} - added");
        }
    }

    if added > 0 {
        write_codex_hooks(&settings)?;
        println!("  Codex configuration written.");
        println!("\n  IMPORTANT: Codex requires one-time trust approval for hooks");
        println!("  it did not create. Launch codex and run: /hooks");
        println!("  then approve the atm-codex-hook entries when prompted.");
    } else {
        println!("  Codex already configured.");
    }
    Ok(())
}

/// Removes atm hooks from Claude Code settings and the hook script
pub fn uninstall() -> Result<()> {
    println!("Uninstalling ATM...\n");

    // Step 1: Remove from Claude Code settings
    println!("Removing Claude Code hooks...");
    let mut settings = read_settings()?;

    let mut removed = 0;
    if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for &hook_type in HOOK_TYPES {
            if let Some(hooks_array) = hooks.get_mut(hook_type).and_then(|h| h.as_array_mut()) {
                let before = hooks_array.len();
                remove_hook_command_marker(hooks_array, "atm-hook");
                let after = hooks_array.len();

                if before != after {
                    removed += before - after;
                    println!("  {hook_type} - removed");
                }

                // Remove empty arrays
                if hooks_array.is_empty() {
                    hooks.remove(hook_type);
                }
            }
        }

        if removed > 0 {
            write_settings(&settings)?;
        }
    }

    if removed == 0 {
        println!("  No hooks found");
    }

    // Step 2: Remove statusLine if it uses atm-hook
    let mut status_line_removed = false;
    if let Some(status_line) = settings.get("statusLine") {
        if has_atm_status_line(status_line) {
            if let Some(obj) = settings.as_object_mut() {
                obj.remove("statusLine");
            }
            write_settings(&settings)?;
            println!("\nstatusLine configuration removed");
            status_line_removed = true;
        }
    }
    if !status_line_removed {
        println!("\nstatusLine - not configured by atm");
    }

    // Step 3: Remove the hook script
    let hook_path = hook_script_path();
    print!("\nRemoving hook script {}... ", hook_path.display());
    if remove_script_file(&hook_script_path())? {
        println!("done");
    } else {
        println!("not found");
    }

    // Step 4: Uninstall pi-atm extension if present
    if detect_pi() {
        print!("\nRemoving pi-atm extension... ");
        match uninstall_pi_extension() {
            Ok(true) => println!("done"),
            Ok(false) => println!("not present"),
            Err(e) => println!("failed: {e}"),
        }
    }

    // Step 5: Remove Codex hooks if configured
    if detect_codex() {
        println!("\nRemoving Codex hooks...");
        let mut codex_settings = read_codex_hooks()?;
        let mut codex_removed = 0;
        if let Some(hooks) = codex_settings
            .get_mut("hooks")
            .and_then(|h| h.as_object_mut())
        {
            for &hook_type in CODEX_HOOK_TYPES {
                if let Some(hooks_array) = hooks.get_mut(hook_type).and_then(|h| h.as_array_mut()) {
                    let before = hooks_array.len();
                    remove_hook_command_marker(hooks_array, "atm-codex-hook");
                    let after = hooks_array.len();

                    if before != after {
                        codex_removed += before - after;
                        println!("  {hook_type} - removed");
                    }

                    if hooks_array.is_empty() {
                        hooks.remove(hook_type);
                    }
                }
            }

            if codex_removed > 0 {
                write_codex_hooks(&codex_settings)?;
            }
        }
        if codex_removed == 0 {
            println!("  No hooks found");
        }
    }

    // The script can remain after ~/.codex is removed, so clean it up
    // independently of Codex installation detection.
    print!(
        "\nRemoving codex hook script {}... ",
        codex_hook_script_path().display()
    );
    if remove_script_file(&codex_hook_script_path())? {
        println!("done");
    } else {
        println!("not found");
    }

    println!("\nATM uninstalled successfully!");
    Ok(())
}

#[cfg(test)]
mod codex_hook_tests {
    use super::{create_codex_hook_entry, has_hook_command_marker, remove_hook_command_marker};
    use serde_json::json;

    const MARKER: &str = "atm-codex-hook";

    #[test]
    fn hook_entry_shape_matches_codex_hooks_json_schema() {
        let entry = create_codex_hook_entry();
        // Validated shape: {"hooks": [{"type": "command", "command": ..., "timeout": 5}]}
        let hooks = entry.get("hooks").and_then(|h| h.as_array()).unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(
            hooks[0].get("type").and_then(|t| t.as_str()),
            Some("command")
        );
        assert!(hooks[0]
            .get("command")
            .and_then(|c| c.as_str())
            .is_some_and(|c| c.contains("atm-codex-hook")));
        assert_eq!(hooks[0].get("timeout").and_then(|t| t.as_u64()), Some(5));
        // No matcher key: omitting it fires for all tools (spike-verified).
        assert!(entry.get("matcher").is_none());
    }

    #[test]
    fn detects_and_removes_only_atm_entries() {
        let user_entry = json!({
            "hooks": [{"type": "command", "command": "/usr/bin/my-own-hook"}]
        });
        let mut hooks_array = vec![user_entry.clone(), create_codex_hook_entry()];

        assert!(has_hook_command_marker(&hooks_array, MARKER));
        remove_hook_command_marker(&mut hooks_array, MARKER);
        assert!(!has_hook_command_marker(&hooks_array, MARKER));
        assert_eq!(
            hooks_array,
            vec![user_entry],
            "user's own hook entries must be preserved"
        );
    }

    #[test]
    fn install_is_idempotent_at_the_entry_level() {
        let mut hooks_array = vec![create_codex_hook_entry()];
        // Mirrors setup_codex's guard: an already-present entry is not
        // duplicated.
        if !has_hook_command_marker(&hooks_array, MARKER) {
            hooks_array.push(create_codex_hook_entry());
        }
        assert_eq!(hooks_array.len(), 1);
    }
}

#[cfg(test)]
mod config_tests {
    use super::{default_atm_config_text, ensure_default_atm_config};
    use std::fs;

    #[test]
    fn creates_config_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");

        let created = ensure_default_atm_config(&path).unwrap();

        assert!(created, "expected newly-created config to report true");
        let on_disk = fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, default_atm_config_text());
    }

    #[test]
    fn preserves_existing_user_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let user_content = "[harness]\ndefault = \"pi\"\n# my custom edits\n";
        fs::write(&path, user_content).unwrap();

        let created = ensure_default_atm_config(&path).unwrap();

        assert!(!created, "existing file must not be reported as created");
        let on_disk = fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, user_content, "user edits must be preserved");
    }

    #[test]
    fn second_call_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let first = ensure_default_atm_config(&path).unwrap();
        let second = ensure_default_atm_config(&path).unwrap();

        assert!(first);
        assert!(!second);
    }
}
