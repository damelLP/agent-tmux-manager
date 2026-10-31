//! Codex CLI: the `atm-codex-hook` script and `hooks.json`.

use std::path::PathBuf;

use atm_codex_adapter::CodexEventType;
use serde_json::{json, Map, Value};

use super::hooks::{add_hook_entries, install_executable_script};
use super::json::{read_json_file_or_empty, write_json_file_pretty};
use super::{Result, SetupError};

/// The atm-codex-hook bash script content (Codex CLI), embedded at compile time.
const ATM_CODEX_HOOK_SCRIPT: &str = include_str!("../../scripts/atm-codex-hook");

/// Returns the path to Codex's hooks.json
fn codex_hooks_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("hooks.json"))
}

/// Returns the path to the atm-codex-hook script
pub(super) fn codex_hook_script_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".local").join("bin").join("atm-codex-hook"))
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/atm-codex-hook"))
}

/// True if the Codex CLI appears to be installed for this user.
///
/// Codex creates `~/.codex/` on first run (auth.json, config.toml,
/// sessions/), regardless of where its binary lives.
pub(super) fn detect_codex() -> bool {
    dirs::home_dir()
        .map(|h| h.join(".codex").exists())
        .unwrap_or(false)
}

/// Reads Codex's hooks.json. Returns an empty object if not present.
pub(super) fn read_codex_hooks() -> Result<Map<String, Value>> {
    let path = codex_hooks_path().ok_or(SetupError::HomeDirUnknown)?;
    read_json_file_or_empty(&path)
}

/// Writes Codex's hooks.json
pub(super) fn write_codex_hooks(hooks: &Map<String, Value>) -> Result<()> {
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
pub(super) fn create_codex_hook_entry() -> Value {
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

/// Wires `atm-codex-hook` into Codex's `~/.codex/hooks.json`.
pub(super) fn setup_codex() -> Result<()> {
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

#[cfg(test)]
mod codex_hook_tests {
    use super::create_codex_hook_entry;
    use crate::setup::hooks::{has_hook_command_marker, remove_hook_command_marker};
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
