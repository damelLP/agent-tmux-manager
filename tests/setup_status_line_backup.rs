//! `atm setup` preserving the user's Claude Code statusLine, and
//! `atm uninstall` restoring backups left by older versions that replaced it.
//! A user's current statusLine always wins over a legacy backup.
//!
//! Every run gets its own HOME and XDG dirs in a temp dir, so nothing
//! reaches the real harness configs, a daemon or tmux.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

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
        .env_remove("TMUX_PANE")
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

/// Seeds the state left by an older setup: ATM's hooks and statusLine,
/// plus the user's original statusLine in the legacy backup file.
fn setup_legacy_install(home: &Path) {
    write_json(&home.join(CLAUDE), &json!({"userKey": "kept"}));
    let stdout = run_atm_ok(home, "setup");
    assert!(stdout.contains("statusLine - added"), "{stdout}");
    assert_eq!(
        read_json(&home.join(CLAUDE))["statusLine"],
        json!({"type": "command", "command": home.join(".local/bin/atm-hook")})
    );
    assert!(!home.join(BACKUP).exists());
    write_json(
        &home.join(BACKUP),
        &json!({"statusLine": user_status_line()}),
    );
}

#[test]
fn setup_preserves_the_users_status_line_without_a_backup() {
    let home = tempfile::tempdir().expect("tempdir");
    let home = home.path();
    let settings_path = home.join(CLAUDE);
    let user = json!({"type": "command", "command": "~/bin/atm-hook-wrapper.sh", "padding": 2});
    write_json(
        &settings_path,
        &json!({"userKey": "kept", "statusLine": user}),
    );

    for _ in 0..2 {
        let stdout = run_atm_ok(home, "setup");

        let settings = read_json(&settings_path);
        assert_eq!(
            serde_json::to_vec(&settings["statusLine"]).unwrap(),
            serde_json::to_vec(&user).unwrap()
        );
        assert_eq!(settings["userKey"], "kept");
        assert!(!home.join(BACKUP).exists());
        let advisory: Vec<_> = stdout
            .lines()
            .filter(|line| line.contains("statusLine - kept yours"))
            .collect();
        assert_eq!(advisory.len(), 1, "{stdout}");
        let line = advisory.first().unwrap();
        assert!(
            line.contains("atm reads Claude Code context, model and cost only through atm-hook"),
            "{line}"
        );
        assert!(
            line.contains("in your status script, pass its stdin to atm-hook"),
            "{line}"
        );
        assert!(
            line.contains(&format!(
                "input=$(cat); printf '%s' \"$input\" | '{}'",
                home.join(".local/bin/atm-hook").display()
            )),
            "{line}"
        );
        assert!(
            line.contains("remove your statusLine and run atm setup again"),
            "{line}"
        );
    }

    // All hooks are present: setup must not rewrite even unusual JSON formatting.
    fs::write(&settings_path, format!("\n{}\n", read_json(&settings_path))).unwrap();
    let before = fs::read(&settings_path).unwrap();
    run_atm_ok(home, "setup");
    assert_eq!(fs::read(&settings_path).unwrap(), before);
}

#[test]
fn setup_advisory_forwards_stdin_to_the_configured_hook_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("user's home");
    write_json(
        &home.join(CLAUDE),
        &json!({"statusLine": user_status_line()}),
    );
    let stdout = run_atm_ok(&home, "setup");
    let example = stdout
        .split_once("(e.g. ")
        .expect("setup explains how to forward statusLine stdin")
        .1
        .split_once("), or remove")
        .expect("setup offers removing the custom statusLine")
        .0;
    // Replace the installed hook with a stdin recorder; run the advertised command.
    fs::write(
        home.join(".local/bin/atm-hook"),
        "#!/bin/sh\ncat > \"$HOME/forwarded.json\"\n",
    )
    .unwrap();
    let input = br#"{"session_id":"advisory","model":{"id":"example"}}"#;
    let mut child = Command::new("/bin/sh")
        .args(["-c", example])
        .env("HOME", &home)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(fs::read(home.join("forwarded.json")).unwrap(), input);
}

#[test]
fn uninstall_removes_only_exact_current_and_legacy_entries() {
    for source in ["current", "history", "fallback"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("user's home");
        let settings_path = home.join(CLAUDE);
        write_json(&settings_path, &json!({"userKey": "kept"}));
        run_atm_ok(&home, "setup");
        let mut settings = read_json(&settings_path);
        // c2a89f0's generator wrote this same two-field object, with a raw path.
        let legacy = json!({
            "type": "command",
            "command": if source == "fallback" {
                "/usr/local/bin/atm-hook".to_string()
            } else {
                home.join(".local/bin/atm-hook").to_string_lossy().into_owned()
            }
        });
        if source == "current" {
            assert_eq!(settings["statusLine"], legacy);
        } else {
            settings["statusLine"] = legacy.clone();
            write_json(&settings_path, &settings);
        }
        let before = fs::read(&settings_path).unwrap();
        let setup = run_atm_ok(&home, "setup");
        assert!(
            setup.contains("statusLine - already configured"),
            "{source}: {setup}"
        );
        assert_eq!(fs::read(&settings_path).unwrap(), before);

        let stdout = run_atm_ok(&home, "uninstall");

        assert_eq!(
            stdout
                .lines()
                .filter(|line| line.starts_with("statusLine"))
                .collect::<Vec<_>>(),
            ["statusLine configuration removed"],
            "{source}: {stdout}"
        );
        assert_eq!(
            read_json(&settings_path),
            json!({"userKey": "kept", "hooks": {}})
        );
        assert!(!home.join(BACKUP).exists());
    }
}

#[test]
fn uninstall_keeps_inline_forwarders_with_or_without_a_backup() {
    for with_backup in [false, true] {
        let home = tempfile::tempdir().expect("tempdir");
        let settings_path = home.path().join(CLAUDE);
        let command = home.path().join(".local/bin/atm-hook");
        let custom = json!({
            "type": "command",
            "command": format!("input=$(cat); printf '%s' \"$input\" | '{}'; echo my-status", command.display())
        });
        write_json(
            &settings_path,
            &json!({"userKey": "kept", "statusLine": custom}),
        );
        let before = fs::read(&settings_path).unwrap();
        if with_backup {
            write_json(
                &home.path().join(BACKUP),
                &json!({"statusLine": user_status_line()}),
            );
        }

        let stdout = run_atm_ok(home.path(), "uninstall");

        assert_eq!(fs::read(&settings_path).unwrap(), before, "{stdout}");
        assert_eq!(
            stdout
                .lines()
                .filter(|line| line.starts_with("statusLine"))
                .collect::<Vec<_>>(),
            ["statusLine - kept yours; not an atm-generated entry"],
            "{stdout}"
        );
        assert!(!home.path().join(BACKUP).exists());
    }
}

#[test]
fn setup_and_uninstall_keep_custom_status_lines() {
    for case in [
        "inline",
        "pipeline",
        "chain",
        "wrapper",
        "padding",
        "other-path",
        "former-home",
        "wrong-type",
        "string",
        "null",
    ] {
        for with_backup in [false, true] {
            let dir = tempfile::tempdir().expect("tempdir");
            let home = dir.path().join("user's home");
            let settings_path = home.join(CLAUDE);
            let backup_path = home.join(BACKUP);
            write_json(&settings_path, &json!({"userKey": "kept"}));
            run_atm_ok(&home, "setup");
            let command = home
                .join(".local/bin/atm-hook")
                .to_string_lossy()
                .into_owned();
            let custom = match case {
                "inline" => {
                    json!({"type": "command", "command": format!("input=$(cat); printf '%s' \"$input\" | \"{command}\"; echo my-status")})
                }
                "pipeline" => {
                    json!({"type": "command", "command": format!("echo my-status | \"{command}\"")})
                }
                "chain" => {
                    json!({"type": "command", "command": format!("echo my-status; \"{command}\"")})
                }
                "wrapper" => json!({"type": "command", "command": "~/bin/atm-hook-wrapper.sh"}),
                "padding" => json!({"type": "command", "command": command, "padding": 2}),
                "other-path" => {
                    json!({"type": "command", "command": "/opt/elsewhere/bin/atm-hook"})
                }
                "former-home" => {
                    json!({"type": "command", "command": "/former-home/.local/bin/atm-hook"})
                }
                "wrong-type" => json!({"type": "prompt", "command": command}),
                "string" => json!(command),
                "null" => Value::Null,
                _ => unreachable!("test case"),
            };
            let mut settings = read_json(&settings_path);
            settings["statusLine"] = custom.clone();
            write_json(&settings_path, &settings);
            if with_backup {
                write_json(&backup_path, &json!({"statusLine": user_status_line()}));
            }
            let backup_before = fs::read(&backup_path).ok();
            for _ in 0..2 {
                let before = fs::read(&settings_path).unwrap();
                let stdout = run_atm_ok(&home, "setup");
                assert_eq!(
                    stdout
                        .lines()
                        .filter(|line| line.contains("statusLine - kept yours"))
                        .count(),
                    1,
                    "{case}: {stdout}"
                );
                assert!(
                    !stdout.contains("statusLine - already configured"),
                    "{case}: {stdout}"
                );
                assert_eq!(read_json(&settings_path)["statusLine"], custom, "{case}");
                assert_eq!(fs::read(&settings_path).unwrap(), before, "{case}");
                assert_eq!(fs::read(&backup_path).ok(), backup_before, "{case}");
            }

            let stdout = run_atm_ok(&home, "uninstall");

            assert_eq!(
                read_json(&settings_path)["statusLine"],
                custom,
                "{case}: {stdout}"
            );
            assert_eq!(read_json(&settings_path)["userKey"], "kept");
            assert_eq!(
                stdout
                    .lines()
                    .filter(|line| line.starts_with("statusLine"))
                    .collect::<Vec<_>>(),
                ["statusLine - kept yours; not an atm-generated entry"],
                "{case}: {stdout}"
            );
            assert!(!backup_path.exists(), "{case}: stale backup left behind");
        }
    }
}

#[test]
fn uninstall_without_a_backup_keeps_the_users_status_line() {
    let home = tempfile::tempdir().expect("tempdir");
    let settings_path = home.path().join(CLAUDE);
    write_json(&settings_path, &json!({"statusLine": user_status_line()}));
    let before = fs::read(&settings_path).unwrap();

    let stdout = run_atm_ok(home.path(), "uninstall");

    assert!(
        stdout.contains("statusLine - kept yours; not an atm-generated entry"),
        "{stdout}"
    );
    assert_eq!(fs::read(&settings_path).unwrap(), before);
    assert!(!home.path().join(BACKUP).exists());
}

#[test]
fn uninstall_restores_the_status_line_an_older_setup_replaced() {
    for fallback in [false, true] {
        let home = tempfile::tempdir().expect("tempdir");
        setup_legacy_install(home.path());
        if fallback {
            let settings_path = home.path().join(CLAUDE);
            let mut settings = read_json(&settings_path);
            settings["statusLine"] =
                json!({"type": "command", "command": "/usr/local/bin/atm-hook"});
            write_json(&settings_path, &settings);
        }

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
}

#[test]
fn uninstall_keeps_a_status_line_that_replaced_atms_and_drops_the_backup() {
    let home = tempfile::tempdir().expect("tempdir");
    setup_legacy_install(home.path());
    let settings_path = home.path().join(CLAUDE);
    let mut settings = read_json(&settings_path);
    let newer = json!({"type": "command", "command": "~/bin/newer-status.sh"});
    settings["statusLine"] = newer.clone();
    write_json(&settings_path, &settings);

    let stdout = run_atm_ok(home.path(), "uninstall");

    assert!(
        stdout.contains("statusLine - kept yours; not an atm-generated entry"),
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
    setup_legacy_install(home.path());

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
    setup_legacy_install(home);
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
        ".claude/skills/atm-coordinate",
        ".claude/skills/atm-implement",
        ".pi/agent/skills/atm-coordinate",
        ".pi/agent/skills/atm-implement",
        ".codex/skills/atm-coordinate",
        ".codex/skills/atm-implement",
    ] {
        assert!(!home.join(artifact).exists(), "{artifact} left: {stdout}");
    }
}

/// With no statusLine left to restore over (settings.json lost or reset
/// since setup), the backup may be the only copy, so uninstall keeps it.
#[test]
fn uninstall_keeps_the_backup_when_settings_were_lost() {
    let home = tempfile::tempdir().expect("tempdir");
    setup_legacy_install(home.path());
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
    setup_legacy_install(home.path());
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
