//! `atm setup` and `atm uninstall` against harness settings files that are
//! valid JSON but not an object.
//!
//! Every run gets its own HOME and XDG dirs in a temp dir, so nothing
//! reaches the real harness configs, a daemon or tmux.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const CLAUDE: &str = ".claude/settings.json";
const CODEX: &str = ".codex/hooks.json";
const PI: &str = ".pi/agent/settings.json";

/// Valid JSON roots that are not objects. The trailing newline is never
/// written by serde_json, so a rewrite of the file can't match these bytes.
const NON_OBJECT_ROOTS: [&str; 4] = ["[]\n", "null\n", "1\n", "\"x\"\n"];

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

/// Writes `content` to `file` (relative to `home`), creating the harness
/// directory so setup detects that harness.
fn write_home_file(home: &Path, file: &str, content: &str) {
    let path = home.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Runs `atm <cmd>` once per non-object root in `file` and returns, for
/// each root, the output and the file's bytes afterwards.
fn run_with_each_root(cmd: &str, file: &str) -> Vec<(&'static str, Output, Vec<u8>)> {
    NON_OBJECT_ROOTS
        .iter()
        .map(|root| {
            let home = tempfile::tempdir().expect("tempdir");
            write_home_file(home.path(), file, root);
            let output = run_atm(home.path(), cmd);
            let after = fs::read(home.path().join(file)).unwrap();
            (*root, output, after)
        })
        .collect()
}

/// Asserts `atm <cmd>` fails with exit 1 (not a panic's 101) and an error
/// naming `file`, and leaves `file` byte-identical, for every non-object
/// root.
fn assert_rejected(cmd: &str, file: &str) {
    let message = format!("~/{file} is not a JSON object");
    for (root, output, after) in run_with_each_root(cmd, file) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(1),
            "atm {cmd} with {file} = {root:?}: {stderr}"
        );
        assert!(
            stderr.contains(&message),
            "atm {cmd} with {file} = {root:?}: {stderr}"
        );
        assert_eq!(
            after,
            root.as_bytes(),
            "atm {cmd} rewrote {file} = {root:?}"
        );
    }
}

#[test]
fn setup_rejects_a_non_object_claude_settings_file() {
    assert_rejected("setup", CLAUDE);
}

#[test]
fn setup_rejects_a_non_object_codex_hooks_file() {
    assert_rejected("setup", CODEX);
}

#[test]
fn setup_rejects_a_non_object_pi_settings_file() {
    assert_rejected("setup", PI);
}

#[test]
fn uninstall_rejects_a_non_object_claude_settings_file() {
    assert_rejected("uninstall", CLAUDE);
}

#[test]
fn uninstall_rejects_a_non_object_codex_hooks_file() {
    assert_rejected("uninstall", CODEX);
}

/// Uninstall prints pi failures and carries on, so it exits 0 (a known
/// limit tracked separately). The error must still name the file, and
/// the file must be left alone.
#[test]
fn uninstall_reports_a_non_object_pi_settings_file() {
    let message = format!("failed: ~/{PI} is not a JSON object");
    for (root, output, after) in run_with_each_root("uninstall", PI) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&message),
            "atm uninstall with {PI} = {root:?}: {stdout}"
        );
        assert_eq!(after, root.as_bytes(), "atm uninstall rewrote {PI}");
    }
}

/// Guards the happy path: setup adds ATM's entries next to the user's own
/// keys in every harness file, and uninstall removes only ATM's entries.
#[test]
fn setup_then_uninstall_keeps_the_users_own_keys() {
    let home = tempfile::tempdir().expect("tempdir");
    let files = [
        (CLAUDE, "atm-hook"),
        (CODEX, "atm-codex-hook"),
        (PI, "pi-atm"),
    ];
    for (file, _) in files {
        write_home_file(home.path(), file, r#"{"userKey": "kept"}"#);
    }

    let setup = run_atm(home.path(), "setup");
    assert!(
        setup.status.success(),
        "setup: {}",
        String::from_utf8_lossy(&setup.stderr)
    );
    for (file, marker) in files {
        let content = fs::read_to_string(home.path().join(file)).unwrap();
        assert!(content.contains(marker), "setup left {file}: {content}");
        assert!(
            content.contains("\"userKey\""),
            "setup lost {file}: {content}"
        );
    }

    let uninstall = run_atm(home.path(), "uninstall");
    assert!(
        uninstall.status.success(),
        "uninstall: {}",
        String::from_utf8_lossy(&uninstall.stderr)
    );
    for (file, marker) in files {
        let content = fs::read_to_string(home.path().join(file)).unwrap();
        assert!(
            !content.contains(marker),
            "uninstall left {file}: {content}"
        );
        assert!(
            content.contains("\"userKey\""),
            "uninstall lost {file}: {content}"
        );
    }
}
