//! Claude Code: the `atm-hook` script, `settings.json` hooks and the statusLine.

use std::path::{Path, PathBuf};

use atm_claude_adapter::ClaudeEventType;
use serde_json::{json, Map, Value};

use super::atm_config_path;
use super::hooks::{add_hook_entries, install_executable_script, is_marker_command};
use super::json::{read_json_file_or_empty, write_json_file_pretty};
use super::{Result, SetupError};

/// The atm-hook bash script content (Claude Code), embedded at compile time.
const ATM_HOOK_SCRIPT: &str = include_str!("../../scripts/atm-hook");

/// Returns the path to Claude Code settings.json
fn claude_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// Returns the path to the atm-hook script
pub(super) fn hook_script_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".local").join("bin").join("atm-hook"))
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/atm-hook"))
}

/// Reads Claude Code settings, returns empty object if file doesn't exist
pub(super) fn read_settings() -> Result<Map<String, Value>> {
    let path = claude_settings_path().ok_or(SetupError::HomeDirUnknown)?;
    read_json_file_or_empty(&path)
}

/// Writes Claude Code settings
pub(super) fn write_settings(settings: &Map<String, Value>) -> Result<()> {
    let path = claude_settings_path().ok_or(SetupError::HomeDirUnknown)?;
    write_json_file_pretty(&path, settings)
}

/// Creates the statusLine configuration entry.
///
/// Uses the same atm-hook script which auto-detects message type.
pub(super) fn create_status_line_entry() -> Value {
    let hook_path = hook_script_path();
    let command = hook_path.to_string_lossy().to_string();

    json!({
        "type": "command",
        "command": command
    })
}

/// Where setup saves the user's own Claude Code statusLine when it
/// replaces it, for uninstall to restore.
pub(super) fn status_line_backup_path() -> Option<PathBuf> {
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
pub(super) fn uninstall_status_line(
    settings: &mut Map<String, Value>,
    backup: Option<Value>,
) -> bool {
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
pub(super) fn has_atm_status_line(status_line: &Value) -> bool {
    is_marker_command(status_line, "atm-hook")
}

/// Creates a hook entry for the given hook type.
///
/// Hook types that filter by tool name use a matcher, others don't.
pub(super) fn create_hook_entry(hook_type: &str) -> Value {
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

/// True if Claude Code appears to be installed for this user.
///
/// We treat the existence of `~/.claude/` as authoritative — Claude
/// creates this directory on first run regardless of where its
/// binary lives.
pub(super) fn detect_claude_code() -> bool {
    dirs::home_dir()
        .map(|h| h.join(".claude").exists())
        .unwrap_or(false)
}

/// Wires `atm-hook` into Claude Code's `~/.claude/settings.json`.
pub(super) fn setup_claude_code() -> Result<()> {
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
