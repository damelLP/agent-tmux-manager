//! Hook configuration for Claude Code integration
//!
//! Manages installation and removal of atm hooks in Claude Code settings.
//! Also handles installing the atm-hook script to ~/.local/bin/.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// The atm-hook script content, embedded at compile time.
const ATM_HOOK_SCRIPT: &str = include_str!("../scripts/atm-hook");

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

/// Returns the path to Claude Code settings.json
fn claude_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// Returns the path to the atm-hook script
fn hook_script_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".local").join("bin").join("atm-hook"))
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/atm-hook"))
}

/// Reads Claude Code settings, returns empty object if file doesn't exist
fn read_settings() -> Result<Value> {
    let path = claude_settings_path().context("Could not determine home directory")?;

    if !path.exists() {
        return Ok(json!({}));
    }

    let content =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;

    serde_json::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))
}

/// Writes Claude Code settings
fn write_settings(settings: &Value) -> Result<()> {
    let path = claude_settings_path().context("Could not determine home directory")?;

    // Ensure .claude directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let content = serde_json::to_string_pretty(settings)?;
    fs::write(&path, content).with_context(|| format!("Failed to write {}", path.display()))
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

/// Checks if atm hooks are already installed for a hook type
fn has_atm_hook(hooks_array: &[Value]) -> bool {
    hooks_array.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(|cmd| cmd.contains("atm-hook"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    })
}

/// Removes atm hooks from a hooks array
fn remove_atm_hooks(hooks_array: &mut Vec<Value>) {
    hooks_array.retain(|entry| {
        !entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(|cmd| cmd.contains("atm-hook"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });
}

/// Installs the atm-hook script to ~/.local/bin/
///
/// Creates the directory if it doesn't exist and sets executable permissions.
fn install_hook_script() -> Result<()> {
    let hook_path = hook_script_path();

    // Create parent directory if needed
    if let Some(parent) = hook_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    // Write the script
    fs::write(&hook_path, ATM_HOOK_SCRIPT)
        .with_context(|| format!("Failed to write {}", hook_path.display()))?;

    // Make executable on Unix
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook_path, perms)
            .with_context(|| format!("Failed to set permissions on {}", hook_path.display()))?;
    }

    Ok(())
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

/// Removes the atm-hook script from ~/.local/bin/
fn remove_hook_script() -> Result<bool> {
    let hook_path = hook_script_path();

    if hook_path.exists() {
        fs::remove_file(&hook_path)
            .with_context(|| format!("Failed to remove {}", hook_path.display()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Installs atm hooks into Claude Code settings
pub fn setup() -> Result<()> {
    println!("Setting up ATM...\n");

    // Step 1: Install the hook script
    let hook_path = hook_script_path();
    print!("Installing hook script to {}... ", hook_path.display());
    install_hook_script()?;
    println!("done");

    // Step 2: Configure Claude Code settings
    println!("\nConfiguring Claude Code hooks...");
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

        if has_atm_hook(hooks_array) {
            println!("  {hook_type} - already configured");
        } else {
            hooks_array.push(create_hook_entry(hook_type));
            added += 1;
            println!("  {hook_type} - added");
        }
    }

    // Step 3: Configure statusLine
    println!("\nConfiguring statusLine...");
    let status_line_configured = if let Some(existing) = settings.get("statusLine") {
        if has_atm_status_line(existing) {
            println!("  statusLine - already configured");
            false
        } else {
            // Replace existing statusLine with atm-hook
            settings["statusLine"] = create_status_line_entry();
            println!("  statusLine - updated to use atm-hook");
            true
        }
    } else {
        settings["statusLine"] = create_status_line_entry();
        println!("  statusLine - added");
        true
    };

    if added > 0 || status_line_configured {
        write_settings(&settings)?;
        println!("\nConfiguration complete!");
    } else {
        println!("\nAll settings already configured.");
    }

    // Step 4: Install tmux keybindings
    println!();
    install_tmux_bindings()?;

    println!("\nNext step:");
    println!("  Run: atm");

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
                remove_atm_hooks(hooks_array);
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
    if remove_hook_script()? {
        println!("done");
    } else {
        println!("not found");
    }

    println!("\nATM uninstalled successfully!");
    Ok(())
}
