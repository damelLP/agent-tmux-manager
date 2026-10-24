//! `atm uninstall` after the user added their own command to a hook
//! entry that `atm setup` wrote. Uninstall must remove only ATM's
//! command and keep the entry, its matcher and the user's command.
//!
//! Every run gets its own HOME and XDG dirs in a temp dir, so nothing
//! reaches the real harness configs, a daemon or tmux.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};

/// Runs `atm <cmd>` with HOME as the isolated home directory.
fn run_atm(home: &Path, cmd: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atm"))
        .arg(cmd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env_remove("TMUX")
        .output()
        .expect("run atm")
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// A command of the user's own, added by hand to ATM's entry.
fn user_hook() -> Value {
    json!({"type": "command", "command": "/usr/bin/my-own-hook"})
}

/// Sets up ATM, appends the user's command to ATM's `event` entry in
/// `file` (relative to HOME), uninstalls, and checks that the entry
/// survives with every field intact and only the user's command left.
fn assert_uninstall_keeps_user_command(file: &str, marker: &str, event: &str) {
    let home = tempfile::tempdir().expect("tempdir");
    let path = home.path().join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();

    let setup = run_atm(home.path(), "setup");
    assert!(setup.status.success(), "setup: {setup:?}");

    let mut settings = read_json(&path);
    let entries = settings["hooks"][event].as_array_mut().unwrap();
    assert_eq!(entries.len(), 1, "setup wrote {entries:?}");
    let entry = &mut entries[0];
    entry["hooks"].as_array_mut().unwrap().push(user_hook());
    let mut expected = entry.clone();
    expected["hooks"] = json!([user_hook()]);
    fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

    let uninstall = run_atm(home.path(), "uninstall");
    let stdout = String::from_utf8_lossy(&uninstall.stdout);
    assert!(uninstall.status.success(), "uninstall: {uninstall:?}");
    assert!(stdout.contains(&format!("{event} - removed")), "{stdout}");

    let after = read_json(&path);
    assert_eq!(after["hooks"][event], json!([expected]), "{file}: {after}");
    assert!(!after.to_string().contains(marker), "{file}: {after}");
}

#[test]
fn claude_uninstall_keeps_a_user_command_in_atms_entry() {
    assert_uninstall_keeps_user_command(".claude/settings.json", "atm-hook", "PreToolUse");
}

#[test]
fn codex_uninstall_keeps_a_user_command_in_atms_entry() {
    assert_uninstall_keeps_user_command(".codex/hooks.json", "atm-codex-hook", "PreToolUse");
}
