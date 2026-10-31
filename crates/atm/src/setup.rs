//! Setup for ATM integrations across coding-agent harnesses.
//!
//! Detects which harnesses are installed (Claude Code, pi, Codex CLI,
//! future) and wires the matching hook for each:
//!
//! - **Claude Code**: writes the `atm-hook` bash script to
//!   `~/.local/bin/`, then registers it in `~/.claude/settings.json`'s
//!   `hooks` and `statusLine` blocks. A statusLine of the user's own is
//!   saved first to `claude-statusline-backup.json` in ATM's config dir
//!   (`~/.config/atm/` on Linux), and `atm uninstall` restores it.
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
//!   instruction for this. codex-cli 0.163.0 also asks at startup
//!   ("Hooks need review"), and `/hooks` still exists.

use std::fs;
use std::io::{self, ErrorKind};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use atm_claude_adapter::ClaudeEventType;
use atm_codex_adapter::CodexEventType;
use serde_json::{json, Map, Value};
use thiserror::Error;

/// Errors from installing or removing ATM's harness integrations.
#[derive(Debug, Error)]
pub enum SetupError {
    /// The home directory could not be determined.
    #[error("Could not determine home directory")]
    HomeDirUnknown,

    /// The user config directory could not be determined.
    #[error("could not determine config directory")]
    ConfigDirUnknown,

    /// A filesystem operation (`op`, e.g. "read") on `path` failed.
    #[error("Failed to {op} {}", .path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Parsing or serializing the JSON file at `path` failed.
    #[error("Failed to {op} {}", .path.display())]
    Json {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// A settings file is valid JSON but not the shape ATM edits.
    #[error("{0}")]
    Shape(&'static str),

    /// The settings file at `path` is valid JSON but not an object.
    #[error("{} is not a JSON object", .path.display())]
    NotAnObject { path: PathBuf },
}

type Result<T> = std::result::Result<T, SetupError>;

/// Maps an io error from `op` on `path` into [`SetupError::Io`].
fn io_err<'a>(op: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> SetupError + 'a {
    move |source| SetupError::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
}

/// Maps a serde_json error from `op` on `path` into [`SetupError::Json`].
fn json_err<'a>(
    op: &'static str,
    path: &'a Path,
) -> impl FnOnce(serde_json::Error) -> SetupError + 'a {
    move |source| SetupError::Json {
        op,
        path: path.to_path_buf(),
        source,
    }
}

/// The atm-hook bash script content (Claude Code), embedded at compile time.
const ATM_HOOK_SCRIPT: &str = include_str!("../scripts/atm-hook");

/// The atm-codex-hook bash script content (Codex CLI), embedded at compile time.
const ATM_CODEX_HOOK_SCRIPT: &str = include_str!("../scripts/atm-codex-hook");

/// The pi-atm TypeScript extension content, embedded at compile time.
/// pi loads `.ts` files directly via `@mariozechner/jiti`.
/// `assets/pi-atm/` mirrors `extensions/pi-atm/`; `tests/setup_pi_package.rs` enforces it.
const PI_ATM_EXTENSION: &str = include_str!("../assets/pi-atm/extensions/pi-atm.ts");

/// Package manifest written next to the embedded extension. Pi looks
/// at the `pi.extensions` array (not `main`) to discover extension
/// files within an installed package.
const PI_ATM_PACKAGE_JSON: &str = include_str!("../assets/pi-atm/package.json");

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

/// Reads the JSON object in the file at `path`, returning an empty object
/// if the file does not exist. Errors carry the path for diagnostics.
///
/// Valid JSON that isn't an object (`[]`, `null`, `1`) is a
/// [`SetupError::NotAnObject`], so callers never panic indexing it or
/// replace the user's file with `{}`.
fn read_json_file_or_empty(path: &Path) -> Result<Map<String, Value>> {
    if !path.exists() {
        return Ok(Map::new());
    }

    let content = fs::read_to_string(path).map_err(io_err("read", path))?;

    match serde_json::from_str(&content).map_err(json_err("parse", path))? {
        Value::Object(settings) => Ok(settings),
        _ => Err(SetupError::NotAnObject {
            path: path.to_path_buf(),
        }),
    }
}

/// Uninstall's read of a harness settings file. One that isn't a JSON
/// object holds no ATM entries, so it reads as empty (and, with nothing
/// removed, is never written back) rather than stopping the uninstall.
fn or_empty_if_not_object(read: Result<Map<String, Value>>) -> Result<Map<String, Value>> {
    match read {
        Err(SetupError::NotAnObject { .. }) => Ok(Map::new()),
        read => read,
    }
}

/// Writes `value` to `path` as pretty-printed JSON, creating parent
/// directories as needed. Errors carry the path for diagnostics.
fn write_json_file_pretty(path: &Path, value: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_err("create", parent))?;
    }

    let content = serde_json::to_string_pretty(value).map_err(json_err("serialize", path))?;
    fs::write(path, content).map_err(io_err("write", path))
}

/// Reads Claude Code settings, returns empty object if file doesn't exist
fn read_settings() -> Result<Map<String, Value>> {
    let path = claude_settings_path().ok_or(SetupError::HomeDirUnknown)?;
    read_json_file_or_empty(&path)
}

/// Writes Claude Code settings
fn write_settings(settings: &Map<String, Value>) -> Result<()> {
    let path = claude_settings_path().ok_or(SetupError::HomeDirUnknown)?;
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

/// Where setup saves the user's own Claude Code statusLine when it
/// replaces it, for uninstall to restore.
fn status_line_backup_path() -> Option<PathBuf> {
    atm_config_path().map(|config| config.with_file_name("claude-statusline-backup.json"))
}

/// Setup's statusLine step: puts ATM's statusLine in `settings`. A
/// statusLine of the user's own is first saved to `backup_path` as
/// `{"statusLine": <theirs>}`; if that fails, `settings` is untouched.
/// With no statusLine, an existing backup is kept: settings.json may
/// have been lost or reset since, leaving the backup as the only copy.
/// Returns whether `settings` changed.
fn install_status_line(settings: &mut Map<String, Value>, backup_path: &Path) -> Result<bool> {
    match settings.get("statusLine") {
        Some(existing) if has_atm_status_line(existing) => {
            println!("    statusLine - already configured");
            return Ok(false);
        }
        Some(existing) => {
            let backup = Map::from_iter([("statusLine".to_string(), existing.clone())]);
            write_json_file_pretty(backup_path, &backup)?;
            println!(
                "    statusLine - replaced yours; saved to {}; `atm uninstall` restores it",
                backup_path.display()
            );
        }
        None if backup_path.exists() => println!(
            "    statusLine - added; your earlier one is still saved in {}, and `atm uninstall` restores it",
            backup_path.display()
        ),
        None => println!("    statusLine - added"),
    }
    settings.insert("statusLine".to_string(), create_status_line_entry());
    Ok(true)
}

/// Uninstall's statusLine step: replaces ATM's statusLine in `settings`
/// with `backup` (the user's own, saved by setup), or removes it when
/// there is none. Any other statusLine replaced ATM's after setup, so it
/// stays. Returns whether `settings` changed.
#[must_use]
fn uninstall_status_line(settings: &mut Map<String, Value>, backup: Option<Value>) -> bool {
    if !settings.get("statusLine").is_some_and(has_atm_status_line) {
        return false;
    }
    match backup {
        Some(original) => settings.insert("statusLine".to_string(), original),
        None => settings.remove("statusLine"),
    };
    true
}

/// Checks if atm-hook is configured for statusLine
fn has_atm_status_line(status_line: &Value) -> bool {
    is_marker_command(status_line, "atm-hook")
}

/// Splits `cmd` into words roughly as a POSIX shell would, expanding
/// nothing: quotes group and are dropped, a backslash escapes the next
/// character (inside double quotes only `$`, `` ` ``, `"` and `\`; none
/// inside single quotes), backslash-newline joins lines, and an unquoted
/// space, tab, newline (POSIX's default IFS) or one of `;&|()<>` ends a
/// word. Other Unicode whitespace, such as NBSP, stays inside a word.
fn shell_words(cmd: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '\\') => match chars.next() {
                Some('\n') => {}
                next => word.get_or_insert_default().extend(next),
            },
            (Some('"'), '\\') => {
                let word = word.get_or_insert_default();
                match chars.next() {
                    Some('\n') => {}
                    Some(c @ ('$' | '`' | '"' | '\\')) => word.push(c),
                    next => word.extend(std::iter::once('\\').chain(next)),
                }
            }
            (Some(_), c) => word.get_or_insert_default().push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                word.get_or_insert_default();
            }
            (None, c) if " \t\n;&|()<>".contains(c) => words.extend(word.take()),
            (None, c) => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    words
}

/// True if `hook`'s command runs the script named `marker`: one of its
/// [`shell_words`] has `marker` as its file name. `/x/atm-hook`,
/// `"/x y/atm-hook"`, `ATM_DEBUG=1 /x/atm-hook` and `/x/atm-hook; :`
/// match; `/x/atm-hook-wrapper.sh` and `'/x/atm-hook wrapper.sh'` don't.
fn is_marker_command(hook: &Value, marker: &str) -> bool {
    hook.get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|cmd| {
            shell_words(cmd)
                .iter()
                .any(|word| Path::new(word).file_name().and_then(|n| n.to_str()) == Some(marker))
        })
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

/// Checks if a hook entry with a command that runs `marker` is present
/// in a hooks array. `marker` is the vendor script's basename
/// (`atm-hook`, `atm-codex-hook`); see [`is_marker_command`].
fn has_hook_command_marker(hooks_array: &[Value], marker: &str) -> bool {
    hooks_array.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| hooks.iter().any(|hook| is_marker_command(hook, marker)))
            .unwrap_or(false)
    })
}

/// Removes the commands that run `marker` from each entry's inner
/// `hooks` array, keeping the entry's other commands and fields (e.g.
/// `matcher`). Drops an entry only when that leaves its `hooks` empty.
/// Returns how many commands were removed.
#[must_use]
fn remove_hook_command_marker(hooks_array: &mut Vec<Value>, marker: &str) -> usize {
    let mut removed = 0;
    hooks_array.retain_mut(|entry| {
        let Some(hooks) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
            return true;
        };
        let before = hooks.len();
        hooks.retain(|hook| !is_marker_command(hook, marker));
        removed += before - hooks.len();
        hooks.len() == before || !hooks.is_empty()
    });
    removed
}

/// Setup's hook loop: for each of `hook_names`, adds `new_entry`'s entry to
/// that event's array in `settings`' `hooks` object unless a command there
/// already runs `marker`, printing the outcome. `not_object` is the error
/// for a `hooks` that isn't an object. Returns how many entries were added.
fn add_hook_entries(
    settings: &mut Map<String, Value>,
    hook_names: impl Iterator<Item = &'static str>,
    marker: &str,
    not_object: &'static str,
    new_entry: impl Fn(&str) -> Value,
) -> Result<usize> {
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or(SetupError::Shape(not_object))?;

    let mut added = 0;

    for hook_type in hook_names {
        let hooks_array = hooks
            .entry(hook_type)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or(SetupError::Shape("hook type is not an array"))?;

        if has_hook_command_marker(hooks_array, marker) {
            println!("    {hook_type} - already configured");
        } else {
            hooks_array.push(new_entry(hook_type));
            added += 1;
            println!("    {hook_type} - added");
        }
    }

    Ok(added)
}

/// Uninstall's hook loop: for each of `hook_names`, removes the commands
/// that run `marker` from that event's array in `settings`' `hooks` object,
/// printing the events that lost one, and drops arrays left empty. Returns
/// how many commands were removed; the caller writes `settings` back.
#[must_use]
fn remove_hook_entries(
    settings: &mut Map<String, Value>,
    hook_names: impl Iterator<Item = &'static str>,
    marker: &str,
) -> usize {
    let mut removed = 0;
    if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for hook_type in hook_names {
            if let Some(hooks_array) = hooks.get_mut(hook_type).and_then(|h| h.as_array_mut()) {
                let count = remove_hook_command_marker(hooks_array, marker);

                if count > 0 {
                    removed += count;
                    println!("  {hook_type} - removed");
                }

                // Remove empty arrays
                if hooks_array.is_empty() {
                    hooks.remove(hook_type);
                }
            }
        }
    }
    removed
}

/// Writes `content` to `path` (creating parent directories as needed)
/// and marks it executable.
fn install_executable_script(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_err("create", parent))?;
    }

    fs::write(path, content).map_err(io_err("write", path))?;

    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .map_err(io_err("set permissions on", path))?;
    }

    Ok(())
}

/// Removes the script at `path`, reporting whether it existed.
fn remove_script_file(path: &Path) -> Result<bool> {
    if path.exists() {
        fs::remove_file(path).map_err(io_err("remove", path))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Installs the ATM tmux keybindings file to ~/.config/atm/tmux-bindings.conf.
fn install_tmux_bindings() -> Result<()> {
    let config_dir = dirs::config_dir()
        .ok_or(SetupError::ConfigDirUnknown)?
        .join("atm");

    std::fs::create_dir_all(&config_dir).map_err(io_err("create", &config_dir))?;

    let bindings_path = config_dir.join("tmux-bindings.conf");
    let content = r#"# ATM — Agent Tmux Manager bindings
# Source this in your .tmux.conf: source-file ~/.config/atm/tmux-bindings.conf

# Spawn a new agent (default: below current pane)
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

    std::fs::write(&bindings_path, content).map_err(io_err("write", &bindings_path))?;
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
fn read_codex_hooks() -> Result<Map<String, Value>> {
    let path = codex_hooks_path().ok_or(SetupError::HomeDirUnknown)?;
    read_json_file_or_empty(&path)
}

/// Writes Codex's hooks.json
fn write_codex_hooks(hooks: &Map<String, Value>) -> Result<()> {
    let path = codex_hooks_path().ok_or(SetupError::HomeDirUnknown)?;
    write_json_file_pretty(&path, hooks)
}

/// Creates a Codex hook entry for the given hook type.
///
/// Shape validated against codex-cli 0.146.1 and, on 2026-10-24, 0.163.0
/// (all 11 events registered): entries live under a
/// top-level `hooks` object keyed by event name; omitting `matcher`
/// fires for all tools. Codex clamps the SessionEnd hook timeout to
/// 3s (0.163.0 warns that it does); 5s is a comfortable ceiling for the
/// fire-and-forget script everywhere else.
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
fn read_pi_settings() -> Result<Map<String, Value>> {
    let path = pi_settings_path().ok_or(SetupError::HomeDirUnknown)?;
    read_json_file_or_empty(&path)
}

fn write_pi_settings(settings: &Map<String, Value>) -> Result<()> {
    let path = pi_settings_path().ok_or(SetupError::HomeDirUnknown)?;
    write_json_file_pretty(&path, settings)
}

/// Writes the embedded pi-atm extension files to
/// `~/.pi/agent/packages/pi-atm/` (overwriting if present), then
/// ensures `"packages/pi-atm"` is in pi's settings `packages` array.
///
/// Returns `(files_written, settings_changed)` for caller's success
/// message.
fn install_pi_extension() -> Result<(bool, bool)> {
    let pkg_dir = pi_atm_package_dir().ok_or(SetupError::HomeDirUnknown)?;
    let extensions_dir = pkg_dir.join("extensions");
    fs::create_dir_all(&extensions_dir).map_err(io_err("create", &extensions_dir))?;

    // Layout matches pi-amplike: package.json with pi.extensions
    // pointing at ./extensions/, and the .ts file inside.
    let ts_path = extensions_dir.join("pi-atm.ts");
    let pkg_path = pkg_dir.join("package.json");

    // Always write to refresh any in-place edits the user might have made.
    fs::write(&ts_path, PI_ATM_EXTENSION).map_err(io_err("write", &ts_path))?;
    fs::write(&pkg_path, PI_ATM_PACKAGE_JSON).map_err(io_err("write", &pkg_path))?;

    // Update pi's settings.json packages array.
    let mut settings = read_pi_settings()?;
    let packages = settings
        .entry("packages")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or(SetupError::Shape(
            "packages is not an array in pi settings.json",
        ))?;

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
            fs::remove_dir_all(&pkg_dir).map_err(io_err("remove", &pkg_dir))?;
            changed = true;
        }
    }
    let mut settings = or_empty_if_not_object(read_pi_settings())?;
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
# The default harness is used by `atm workspace create`, and by `atm spawn`
# when it omits `--harness`.
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
        fs::create_dir_all(parent).map_err(io_err("create config directory", parent))?;
    }
    match fs::write(path, default_atm_config_text()) {
        Ok(()) => Ok(true),
        // Lost a race with another writer — treat as already-present.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).map_err(io_err("write default config", path)),
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

    let added = add_hook_entries(
        &mut settings,
        ClaudeEventType::hook_names(),
        "atm-hook",
        "hooks is not an object",
        create_hook_entry,
    )?;

    // statusLine
    let backup_path = status_line_backup_path().ok_or(SetupError::ConfigDirUnknown)?;
    let status_line_configured = install_status_line(&mut settings, &backup_path)?;

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

    let mut settings = read_codex_hooks()?;

    let added = add_hook_entries(
        &mut settings,
        CodexEventType::hook_names(),
        "atm-codex-hook",
        "hooks is not an object in codex hooks.json",
        |_| create_codex_hook_entry(),
    )?;

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
    let mut settings = or_empty_if_not_object(read_settings())?;

    let removed = remove_hook_entries(&mut settings, ClaudeEventType::hook_names(), "atm-hook");

    if removed > 0 {
        write_settings(&settings)?;
    } else {
        println!("  No hooks found");
    }

    // Step 2: Remove statusLine if it uses atm-hook, restoring the user's
    // own from setup's backup. ATM's backup never stops uninstall: one it
    // can't read or delete is left for the user, with a warning.
    let backup_path = status_line_backup_path().ok_or(SetupError::ConfigDirUnknown)?;
    let (backup, readable) = match read_json_file_or_empty(&backup_path) {
        Ok(mut backup) => (backup.remove("statusLine"), true),
        Err(e) => {
            println!(
                "\nWarning: {e}; skipping the statusLine restore and leaving the backup in place"
            );
            (None, false)
        }
    };
    let has_backup = backup.is_some();
    let changed = uninstall_status_line(&mut settings, backup);
    if changed {
        write_settings(&settings)?;
    }
    match (changed, has_backup) {
        (true, true) => println!("\nstatusLine restored from {}", backup_path.display()),
        (true, false) => println!("\nstatusLine configuration removed"),
        (false, _) => println!("\nstatusLine - not configured by atm"),
    }
    // The backup is done with once restored, or once a statusLine of the
    // user's own replaced ATM's. With no statusLine left (settings.json
    // lost or reset) it may be the only copy, so it stays.
    if readable && (changed || settings.contains_key("statusLine")) {
        if let Err(e) = remove_script_file(&backup_path) {
            println!("\nWarning: {e}");
        }
    } else if has_backup {
        println!(
            "  Your earlier statusLine is still saved in {}",
            backup_path.display()
        );
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
        let mut codex_settings = or_empty_if_not_object(read_codex_hooks())?;
        let codex_removed = remove_hook_entries(
            &mut codex_settings,
            CodexEventType::hook_names(),
            "atm-codex-hook",
        );

        if codex_removed > 0 {
            write_codex_hooks(&codex_settings)?;
        } else {
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
        // No matcher key: omitting it fires for all tools (spike-verified;
        // codex-cli 0.163.0 fired it for its one tool used, exec_command).
        assert!(entry.get("matcher").is_none());
    }

    #[test]
    fn detects_and_removes_only_atm_entries() {
        let user_entry = json!({
            "hooks": [{"type": "command", "command": "/usr/bin/my-own-hook"}]
        });
        let mut hooks_array = vec![user_entry.clone(), create_codex_hook_entry()];

        assert!(has_hook_command_marker(&hooks_array, MARKER));
        assert_eq!(remove_hook_command_marker(&mut hooks_array, MARKER), 1);
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
mod shared_entry_tests {
    use super::{
        create_codex_hook_entry, create_hook_entry, create_status_line_entry, has_atm_status_line,
        has_hook_command_marker, is_marker_command, remove_hook_command_marker, shell_words,
    };
    use serde_json::{json, Value};

    const USER_HOOK: &str = "/usr/bin/my-own-hook";

    /// ATM's `entry` with the user's own command added to its inner
    /// `hooks` array, as a user editing settings.json might do.
    fn shared_with_user_hook(mut entry: Value) -> Value {
        entry["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "command", "command": USER_HOOK}));
        entry
    }

    /// Removing `marker` from a shared entry must leave the entry, its
    /// other fields and the user's command, minus only ATM's command.
    fn assert_keeps_user_command(entry: Value, marker: &str) {
        let mut hooks_array = vec![shared_with_user_hook(entry.clone())];
        let mut expected = entry;
        expected["hooks"] = json!([{"type": "command", "command": USER_HOOK}]);

        let removed = remove_hook_command_marker(&mut hooks_array, marker);

        assert_eq!(removed, 1);
        assert_eq!(hooks_array, vec![expected]);
    }

    #[test]
    fn claude_uninstall_keeps_a_user_command_sharing_atms_entry() {
        assert_keeps_user_command(create_hook_entry("PreToolUse"), "atm-hook");
        assert_keeps_user_command(create_hook_entry("SessionStart"), "atm-hook");
    }

    #[test]
    fn codex_uninstall_keeps_a_user_command_sharing_atms_entry() {
        assert_keeps_user_command(create_codex_hook_entry(), "atm-codex-hook");
    }

    #[test]
    fn an_entry_left_empty_is_dropped_but_an_already_empty_one_is_kept() {
        let already_empty = json!({"matcher": "Bash", "hooks": []});
        let mut hooks_array = vec![create_hook_entry("PreToolUse"), already_empty.clone()];

        let removed = remove_hook_command_marker(&mut hooks_array, "atm-hook");

        assert_eq!(removed, 1);
        assert_eq!(hooks_array, vec![already_empty]);
    }

    #[test]
    fn a_script_whose_name_only_contains_the_marker_is_not_atms() {
        let wrapper = json!({
            "hooks": [{"type": "command", "command": "/home/u/bin/atm-hook-wrapper.sh"}]
        });
        let mut hooks_array = vec![wrapper.clone()];

        assert!(!has_hook_command_marker(&hooks_array, "atm-hook"));
        assert_eq!(remove_hook_command_marker(&mut hooks_array, "atm-hook"), 0);
        assert_eq!(hooks_array, vec![wrapper]);
    }

    #[test]
    fn a_command_running_the_script_with_env_args_or_quotes_is_atms() {
        for command in [
            "/home/u/.local/bin/atm-hook",
            "ATM_DEBUG=1 /home/u/.local/bin/atm-hook",
            "/home/u/.local/bin/atm-hook --verbose",
            "\"$HOME/.local/bin/atm-hook\"",
            "'/Users/John Doe/.local/bin/atm-hook'",
            "/Users/John\\ Doe/.local/bin/atm-hook",
            "/home/u/.local/bin/atm-hook; :",
            "/home/u/.local/bin/atm-hook&&true",
            "(/home/u/.local/bin/atm-hook)",
        ] {
            let command = json!({"type": "command", "command": command});
            assert!(is_marker_command(&command, "atm-hook"), "{command}");
        }
    }

    #[test]
    fn a_quoted_script_name_with_a_space_after_the_marker_is_not_atms() {
        for command in [
            "'/home/u/bin/atm-hook wrapper.sh'",
            "\"/home/u/bin/atm-hook wrapper.sh\" --flag",
            "/home/u/bin/atm-hook\\ wrapper.sh",
            "\"/home/u/bin/atm\\-hook\"",
            "/home/u/bin/atm-hook\u{a0}wrapper.sh",
        ] {
            let command = json!({"type": "command", "command": command});
            assert!(!is_marker_command(&command, "atm-hook"), "{command}");
        }
    }

    #[test]
    fn shell_words_groups_quotes_and_splits_on_operators() {
        assert_eq!(
            shell_words(r#"a 'b c' "d\"e" f\ g;h&&i|j (k) <l >m"#),
            ["a", "b c", "d\"e", "f g", "h", "i", "j", "k", "l", "m"]
        );
        assert_eq!(shell_words("x '' y"), ["x", "", "y"]);
        assert_eq!(shell_words("a\tb\nc\u{a0}d"), ["a", "b", "c\u{a0}d"]);
        assert_eq!(shell_words(r"'a\b'"), [r"a\b"]);
    }

    /// POSIX: inside double quotes a backslash escapes only `$`, `` ` ``,
    /// `"`, `\` and newline, and is kept before anything else. A
    /// backslash-newline is a line continuation, quoted or not.
    #[test]
    fn shell_words_keeps_double_quoted_backslashes_posix_keeps() {
        assert_eq!(
            shell_words(r#""a\-b" "c\$d" "e\\f" "g\`h""#),
            [r"a\-b", "c$d", r"e\f", "g`h"]
        );
        assert_eq!(shell_words("a\\\nb \"c\\\nd\""), ["ab", "cd"]);
    }

    #[test]
    fn a_status_line_script_whose_name_only_contains_the_marker_is_not_atms() {
        let wrapper = json!({"type": "command", "command": "/home/u/bin/atm-hook-wrapper.sh"});

        assert!(!has_atm_status_line(&wrapper));
        assert!(has_atm_status_line(&create_status_line_entry()));
    }
}

#[cfg(test)]
mod status_line_backup_tests {
    use super::{create_status_line_entry, install_status_line, uninstall_status_line, SetupError};
    use serde_json::{json, Map, Value};
    use std::fs;
    use std::path::Path;

    /// A statusLine of the user's own, with a field ATM never writes.
    fn user_status_line() -> Value {
        json!({"type": "command", "command": "~/bin/my-status.sh", "padding": 2})
    }

    fn settings_with(status_line: Value) -> Map<String, Value> {
        Map::from_iter([("statusLine".to_string(), status_line)])
    }

    fn write_backup(path: &Path, status_line: Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, json!({ "statusLine": status_line }).to_string()).unwrap();
    }

    fn read_backup(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn install_saves_the_users_status_line_then_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join("atm/backup.json");
        let mut settings = settings_with(user_status_line());

        assert!(install_status_line(&mut settings, &backup).unwrap());

        assert_eq!(settings, settings_with(create_status_line_entry()));
        assert_eq!(
            read_backup(&backup),
            json!({"statusLine": user_status_line()})
        );
    }

    #[test]
    fn install_without_a_status_line_adds_atms_and_saves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join("backup.json");
        let mut settings = Map::new();

        assert!(install_status_line(&mut settings, &backup).unwrap());

        assert_eq!(settings, settings_with(create_status_line_entry()));
        assert!(!backup.exists());
    }

    #[test]
    fn install_over_atms_own_status_line_keeps_the_backup() {
        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join("backup.json");
        write_backup(&backup, user_status_line());
        let mut settings = settings_with(create_status_line_entry());

        assert!(!install_status_line(&mut settings, &backup).unwrap());

        assert_eq!(settings, settings_with(create_status_line_entry()));
        assert_eq!(
            read_backup(&backup),
            json!({"statusLine": user_status_line()})
        );
    }

    #[test]
    fn install_leaves_the_users_status_line_if_the_backup_fails() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("atm");
        fs::write(&blocker, "not a directory").unwrap();
        let mut settings = settings_with(user_status_line());

        let err = install_status_line(&mut settings, &blocker.join("backup.json")).unwrap_err();

        assert!(
            matches!(err, SetupError::Io { op: "create", ref path, .. } if path == &blocker),
            "{err:?}"
        );
        assert_eq!(settings, settings_with(user_status_line()));
    }

    /// A backup may outlive the statusLine it came from: settings.json
    /// may have been lost or reset since. It is then the only copy.
    #[test]
    fn install_without_a_status_line_keeps_an_earlier_backup() {
        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join("backup.json");
        write_backup(&backup, user_status_line());
        let mut settings = Map::new();

        assert!(install_status_line(&mut settings, &backup).unwrap());

        assert_eq!(settings, settings_with(create_status_line_entry()));
        assert_eq!(
            read_backup(&backup),
            json!({"statusLine": user_status_line()})
        );
    }

    #[test]
    fn uninstall_restores_the_backup_over_atms_status_line() {
        let mut settings = settings_with(create_status_line_entry());

        assert!(uninstall_status_line(
            &mut settings,
            Some(user_status_line())
        ));

        assert_eq!(settings, settings_with(user_status_line()));
    }

    #[test]
    fn uninstall_without_a_backup_removes_atms_status_line() {
        let mut settings = settings_with(create_status_line_entry());

        assert!(uninstall_status_line(&mut settings, None));

        assert!(settings.is_empty(), "{settings:?}");
    }

    #[test]
    fn uninstall_keeps_a_status_line_that_replaced_atms() {
        let newer = json!({"type": "command", "command": "~/bin/newer-status.sh"});
        let mut settings = settings_with(newer.clone());

        assert!(!uninstall_status_line(
            &mut settings,
            Some(user_status_line())
        ));

        assert_eq!(settings, settings_with(newer));
    }

    #[test]
    fn uninstall_without_a_status_line_restores_nothing() {
        let mut settings = Map::new();

        assert!(!uninstall_status_line(
            &mut settings,
            Some(user_status_line())
        ));

        assert!(settings.is_empty(), "{settings:?}");
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

#[cfg(test)]
mod error_tests {
    use super::{ensure_default_atm_config, io_err, json_err, SetupError};
    use std::error::Error as _;
    use std::fs;
    use std::io;
    use std::path::Path;

    /// For each `op` string used in this file, Display must match the anyhow
    /// context string setup printed before SetupError existed (`serialize`
    /// is new: that site had no context).
    #[test]
    fn display_matches_previous_messages() {
        let p = Path::new("/h/.claude/settings.json");
        let io = || io::Error::from(io::ErrorKind::PermissionDenied);
        let json = || serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let cases = [
            (
                SetupError::HomeDirUnknown,
                "Could not determine home directory",
            ),
            (
                SetupError::ConfigDirUnknown,
                "could not determine config directory",
            ),
            (
                io_err("read", p)(io()),
                "Failed to read /h/.claude/settings.json",
            ),
            (
                io_err("create", p)(io()),
                "Failed to create /h/.claude/settings.json",
            ),
            (
                io_err("write", p)(io()),
                "Failed to write /h/.claude/settings.json",
            ),
            (
                io_err("set permissions on", p)(io()),
                "Failed to set permissions on /h/.claude/settings.json",
            ),
            (
                io_err("remove", p)(io()),
                "Failed to remove /h/.claude/settings.json",
            ),
            (
                io_err("create config directory", p)(io()),
                "Failed to create config directory /h/.claude/settings.json",
            ),
            (
                io_err("write default config", p)(io()),
                "Failed to write default config /h/.claude/settings.json",
            ),
            (
                json_err("parse", p)(json()),
                "Failed to parse /h/.claude/settings.json",
            ),
            (
                json_err("serialize", p)(json()),
                "Failed to serialize /h/.claude/settings.json",
            ),
            (
                SetupError::Shape("hooks is not an object"),
                "hooks is not an object",
            ),
            (
                SetupError::Shape("hook type is not an array"),
                "hook type is not an array",
            ),
            (
                SetupError::Shape("hooks is not an object in codex hooks.json"),
                "hooks is not an object in codex hooks.json",
            ),
            (
                SetupError::Shape("packages is not an array in pi settings.json"),
                "packages is not an array in pi settings.json",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
        }
    }

    /// The underlying error stays reachable via `source()`, so anyhow in
    /// `main` still prints it under "Caused by:".
    #[test]
    fn io_and_json_errors_keep_their_source() {
        let p = Path::new("/x");
        let io = io_err("read", p)(io::Error::from(io::ErrorKind::NotFound));
        assert_eq!(
            io.source().map(ToString::to_string),
            Some(io::Error::from(io::ErrorKind::NotFound).to_string())
        );

        let parse = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let expected = parse.to_string();
        let json = json_err("parse", p)(parse);
        assert_eq!(json.source().map(ToString::to_string), Some(expected));
    }

    #[test]
    fn ensure_default_atm_config_reports_a_typed_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("atm");
        fs::write(&blocker, "not a directory").unwrap();

        let err = ensure_default_atm_config(&blocker.join("config.toml")).unwrap_err();

        assert!(
            matches!(err, SetupError::Io { op: "create config directory", ref path, .. } if path == &blocker),
            "unexpected error: {err:?}"
        );
    }
}

#[cfg(test)]
mod settings_read_tests {
    use super::{json_err, or_empty_if_not_object, read_json_file_or_empty, SetupError};
    use serde_json::{json, Map, Value};
    use std::fs;
    use std::path::Path;

    #[test]
    fn non_object_roots_are_rejected_naming_their_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        for root in ["[]", "null", "1", "\"x\""] {
            fs::write(&path, root).unwrap();

            let err = read_json_file_or_empty(&path).unwrap_err();

            assert!(
                matches!(err, SetupError::NotAnObject { path: ref p } if p == &path),
                "{root}: {err:?}"
            );
            assert_eq!(
                err.to_string(),
                format!("{} is not a JSON object", path.display())
            );
        }
    }

    #[test]
    fn an_object_root_is_read_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"userKey": "kept"}"#).unwrap();

        let settings = read_json_file_or_empty(&path).unwrap();

        assert_eq!(Value::Object(settings), json!({"userKey": "kept"}));
    }

    #[test]
    fn a_missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();

        let settings = read_json_file_or_empty(&dir.path().join("absent.json")).unwrap();

        assert!(settings.is_empty());
    }

    #[test]
    fn uninstall_reads_a_non_object_file_as_empty() {
        let not_object = SetupError::NotAnObject {
            path: "/h/.claude/settings.json".into(),
        };

        assert_eq!(or_empty_if_not_object(Err(not_object)).unwrap(), Map::new());
    }

    #[test]
    fn uninstall_still_fails_on_other_read_errors() {
        let p = Path::new("/h/.claude/settings.json");
        let parse = serde_json::from_str::<Value>("{").unwrap_err();

        let read = or_empty_if_not_object(Err(json_err("parse", p)(parse)));

        assert!(
            matches!(read, Err(SetupError::Json { op: "parse", .. })),
            "{read:?}"
        );
    }

    #[test]
    fn uninstall_reads_an_object_file_unchanged() {
        let settings = json!({"userKey": "kept"}).as_object().cloned().unwrap();

        assert_eq!(
            or_empty_if_not_object(Ok(settings.clone())).unwrap(),
            settings
        );
    }
}
