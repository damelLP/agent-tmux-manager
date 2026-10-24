//! `atm setup` replacing the user's own Claude Code statusLine, and
//! `atm uninstall` giving it back. Setup saves the user's statusLine to
//! a backup file in ATM's config dir; uninstall restores it over ATM's,
//! unless something replaced ATM's statusLine since.
//!
//! Every run gets its own HOME and XDG dirs in a temp dir, so nothing
//! reaches the real harness configs, a daemon or tmux.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};

const CLAUDE: &str = ".claude/settings.json";
const BACKUP: &str = ".config/atm/claude-statusline-backup.json";

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

/// Runs `atm <cmd>`, asserting success, and returns its stdout.
fn run_atm_ok(home: &Path, cmd: &str) -> String {
    let output = run_atm(home, cmd);
    assert!(output.status.success(), "{cmd}: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

/// A statusLine of the user's own, with a field ATM never writes.
fn user_status_line() -> Value {
    json!({"type": "command", "command": "~/bin/my-status.sh", "padding": 2})
}

/// Writes a Claude settings file holding the user's statusLine, runs
/// setup, and checks ATM's statusLine replaced it and the backup holds it.
fn setup_over_user_status_line(home: &Path) {
    write_json(
        &home.join(CLAUDE),
        &json!({"userKey": "kept", "statusLine": user_status_line()}),
    );

    let stdout = run_atm_ok(home, "setup");

    let backup = home.join(BACKUP);
    assert!(
        stdout.contains(&format!(
            "statusLine - replaced yours; saved to {}",
            backup.display()
        )),
        "{stdout}"
    );
    let settings = read_json(&home.join(CLAUDE));
    let command = settings["statusLine"]["command"].as_str().unwrap();
    assert!(command.ends_with("/atm-hook"), "{settings}");
    assert_eq!(
        read_json(&backup),
        json!({"statusLine": user_status_line()})
    );
}

#[test]
fn uninstall_restores_the_status_line_setup_replaced() {
    let home = tempfile::tempdir().expect("tempdir");
    setup_over_user_status_line(home.path());

    let stdout = run_atm_ok(home.path(), "uninstall");

    let backup = home.path().join(BACKUP);
    assert!(
        stdout.contains(&format!("statusLine restored from {}", backup.display())),
        "{stdout}"
    );
    assert_eq!(
        read_json(&home.path().join(CLAUDE)),
        json!({"userKey": "kept", "statusLine": user_status_line(), "hooks": {}})
    );
    assert!(!backup.exists(), "backup left behind");
}

#[test]
fn uninstall_keeps_a_status_line_that_replaced_atms_and_drops_the_backup() {
    let home = tempfile::tempdir().expect("tempdir");
    setup_over_user_status_line(home.path());
    let settings_path = home.path().join(CLAUDE);
    let mut settings = read_json(&settings_path);
    let newer = json!({"type": "command", "command": "~/bin/newer-status.sh"});
    settings["statusLine"] = newer.clone();
    write_json(&settings_path, &settings);

    let stdout = run_atm_ok(home.path(), "uninstall");

    assert!(
        stdout.contains("statusLine - not configured by atm"),
        "{stdout}"
    );
    assert_eq!(read_json(&settings_path)["statusLine"], newer);
    assert!(
        !home.path().join(BACKUP).exists(),
        "stale backup left behind"
    );
}

#[test]
fn rerunning_setup_keeps_the_backup() {
    let home = tempfile::tempdir().expect("tempdir");
    setup_over_user_status_line(home.path());

    let stdout = run_atm_ok(home.path(), "setup");

    assert!(
        stdout.contains("statusLine - already configured"),
        "{stdout}"
    );
    assert_eq!(
        read_json(&home.path().join(BACKUP)),
        json!({"statusLine": user_status_line()})
    );
}

/// ATM's backup never stops uninstall: a corrupt one is reported, left in
/// place for the user, and ATM's statusLine is removed as with no backup.
/// Codex and pi are set up too, to check the later steps still run.
#[test]
fn a_corrupt_backup_lets_uninstall_finish_and_stays_in_place() {
    let home = tempfile::tempdir().expect("tempdir");
    let home = home.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::create_dir_all(home.join(".pi/agent")).unwrap();
    setup_over_user_status_line(home);
    let backup = home.join(BACKUP);
    fs::write(&backup, "{\n").unwrap();

    let stdout = run_atm_ok(home, "uninstall");

    let warning = format!("Warning: Failed to parse {};", backup.display());
    assert!(stdout.contains(&warning), "{stdout}");
    assert!(
        stdout.contains("statusLine configuration removed"),
        "{stdout}"
    );
    assert!(stdout.contains("ATM uninstalled successfully!"), "{stdout}");
    assert_eq!(fs::read(&backup).unwrap(), b"{\n");
    assert!(read_json(&home.join(CLAUDE)).get("statusLine").is_none());
    let codex = fs::read_to_string(home.join(".codex/hooks.json")).unwrap();
    assert!(!codex.contains("atm-codex-hook"), "{codex}");
    for artifact in [
        ".local/bin/atm-hook",
        ".local/bin/atm-codex-hook",
        ".pi/agent/packages/pi-atm",
    ] {
        assert!(!home.join(artifact).exists(), "{artifact} left: {stdout}");
    }
}

/// With no statusLine left to restore over (settings.json lost or reset
/// since setup), the backup may be the only copy, so uninstall keeps it.
#[test]
fn uninstall_keeps_the_backup_when_settings_were_lost() {
    let home = tempfile::tempdir().expect("tempdir");
    setup_over_user_status_line(home.path());
    fs::remove_file(home.path().join(CLAUDE)).unwrap();

    let stdout = run_atm_ok(home.path(), "uninstall");

    let backup = home.path().join(BACKUP);
    assert!(
        stdout.contains(&format!(
            "Your earlier statusLine is still saved in {}",
            backup.display()
        )),
        "{stdout}"
    );
    assert_eq!(
        read_json(&backup),
        json!({"statusLine": user_status_line()})
    );
}

/// A stale backup uninstall can't delete is reported, not fatal.
#[test]
fn an_undeletable_backup_lets_uninstall_finish() {
    let home = tempfile::tempdir().expect("tempdir");
    setup_over_user_status_line(home.path());
    let settings_path = home.path().join(CLAUDE);
    let mut settings = read_json(&settings_path);
    settings["statusLine"] = json!({"type": "command", "command": "~/bin/newer-status.sh"});
    write_json(&settings_path, &settings);
    let config_dir = home.path().join(".config/atm");
    let mode =
        |m| fs::set_permissions(&config_dir, std::os::unix::fs::PermissionsExt::from_mode(m));
    mode(0o555).unwrap();
    if fs::write(config_dir.join("probe"), "").is_ok() {
        mode(0o755).unwrap();
        eprintln!("skipped: a read-only dir is writable here (root?)");
        return;
    }

    let output = run_atm(home.path(), "uninstall");
    mode(0o755).unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    let backup = home.path().join(BACKUP);
    let warning = format!("Warning: Failed to remove {}", backup.display());
    assert!(stdout.contains(&warning), "{stdout}");
    assert!(stdout.contains("ATM uninstalled successfully!"), "{stdout}");
    assert!(!home.path().join(".local/bin/atm-hook").exists());
}
