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

/// Each harness's settings file and the marker of ATM's entry in it.
const HARNESS_FILES: [(&str, &str); 3] = [
    (CLAUDE, "atm-hook"),
    (CODEX, "atm-codex-hook"),
    (PI, "pi-atm"),
];

/// What `atm setup` installs outside the settings files.
const INSTALLED_ARTIFACTS: [&str; 3] = [
    ".local/bin/atm-hook",
    ".local/bin/atm-codex-hook",
    ".pi/agent/packages/pi-atm",
];

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

/// Asserts `atm setup` fails with exit 1 (not a panic's 101) and an error
/// naming the resolved path of `file`, and leaves `file` byte-identical,
/// for every non-object root.
fn assert_setup_rejects(file: &str) {
    for root in NON_OBJECT_ROOTS {
        let home = tempfile::tempdir().expect("tempdir");
        let path = home.path().join(file);
        write_home_file(home.path(), file, root);

        let output = run_atm(home.path(), "setup");

        let stderr = String::from_utf8_lossy(&output.stderr);
        let context = format!("atm setup with {file} = {root:?}: {stderr}");
        assert_eq!(output.status.code(), Some(1), "{context}");
        let message = format!("{} is not a JSON object", path.display());
        assert!(stderr.contains(&message), "{context}");
        assert_eq!(fs::read(&path).unwrap(), root.as_bytes(), "{context}");
    }
}

/// Sets up every harness, then replaces `file` with each non-object root
/// and runs `atm uninstall`. Such a file holds no ATM entries, so the run
/// must complete, remove ATM everywhere else, and leave `file`
/// byte-identical.
fn assert_uninstall_skips(file: &str) {
    for root in NON_OBJECT_ROOTS {
        let home = tempfile::tempdir().expect("tempdir");
        let home = home.path();
        for (harness_file, _) in HARNESS_FILES {
            write_home_file(home, harness_file, "{}");
        }
        let setup = run_atm(home, "setup");
        assert!(setup.status.success(), "setup: {setup:?}");
        write_home_file(home, file, root);

        let output = run_atm(home, "uninstall");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let context = format!("atm uninstall with {file} = {root:?}: {stdout}{stderr}");
        assert!(output.status.success(), "{context}");
        assert!(
            stdout.contains("ATM uninstalled successfully!"),
            "{context}"
        );
        assert!(!stdout.contains("failed"), "{context}");
        assert_eq!(
            fs::read(home.join(file)).unwrap(),
            root.as_bytes(),
            "{context}"
        );
        for (harness_file, marker) in HARNESS_FILES {
            if harness_file != file {
                let content = fs::read_to_string(home.join(harness_file)).unwrap();
                assert!(!content.contains(marker), "{harness_file}: {content}");
            }
        }
        for artifact in INSTALLED_ARTIFACTS {
            assert!(!home.join(artifact).exists(), "{artifact} left: {context}");
        }
    }
}

#[test]
fn setup_rejects_a_non_object_claude_settings_file() {
    assert_setup_rejects(CLAUDE);
}

#[test]
fn setup_rejects_a_non_object_codex_hooks_file() {
    assert_setup_rejects(CODEX);
}

#[test]
fn setup_rejects_a_non_object_pi_settings_file() {
    assert_setup_rejects(PI);
}

#[test]
fn uninstall_skips_a_non_object_claude_settings_file() {
    assert_uninstall_skips(CLAUDE);
}

#[test]
fn uninstall_skips_a_non_object_codex_hooks_file() {
    assert_uninstall_skips(CODEX);
}

#[test]
fn uninstall_skips_a_non_object_pi_settings_file() {
    assert_uninstall_skips(PI);
}

/// The `userKey` value in a settings file's content.
fn user_key(content: &str) -> serde_json::Value {
    let settings: serde_json::Value = serde_json::from_str(content).unwrap();
    settings["userKey"].clone()
}

/// Guards the happy path: setup adds ATM's entries next to the user's own
/// keys in every harness file, and uninstall removes only ATM's entries.
#[test]
fn setup_then_uninstall_keeps_the_users_own_keys() {
    let home = tempfile::tempdir().expect("tempdir");
    for (file, _) in HARNESS_FILES {
        write_home_file(home.path(), file, r#"{"userKey": "kept"}"#);
    }

    let setup = run_atm(home.path(), "setup");
    assert!(
        setup.status.success(),
        "setup: {}",
        String::from_utf8_lossy(&setup.stderr)
    );
    for (file, marker) in HARNESS_FILES {
        let content = fs::read_to_string(home.path().join(file)).unwrap();
        assert!(content.contains(marker), "setup left {file}: {content}");
        assert_eq!(user_key(&content), "kept", "setup lost {file}: {content}");
    }

    let uninstall = run_atm(home.path(), "uninstall");
    assert!(
        uninstall.status.success(),
        "uninstall: {}",
        String::from_utf8_lossy(&uninstall.stderr)
    );
    for (file, marker) in HARNESS_FILES {
        let content = fs::read_to_string(home.path().join(file)).unwrap();
        assert!(
            !content.contains(marker),
            "uninstall left {file}: {content}"
        );
        assert_eq!(
            user_key(&content),
            "kept",
            "uninstall lost {file}: {content}"
        );
    }
}
