//! `atm spawn`: which pane it splits, and what it prints (#121, gap 1 of
//! #130).
//!
//! `atm` runs with a cleared environment whose `PATH` holds only a
//! stand-in `tmux`. The stand-in logs each call and answers like tmux:
//! `list-panes` lists an active `%1` in another session and the caller's
//! inactive `%7` and `%9`, `display-message` gives the pane's path,
//! `split-window` prints the new pane `%42`, and a split of any other pane
//! fails with tmux's "can't find pane". Nothing reaches a real tmux server
//! or harness: the typed command goes to the stand-in.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const STAND_IN_TMUX: &str = r#"#!/bin/sh
echo "$*" >> 'DIR/tmux.log'
# $1 is -u; the subcommand comes next.
case $2 in
  list-panes)
    printf '%%1\tother\t0\t100\t80\t24\t1\n%%7\tmine\t1\t101\t80\t24\t0\n%%9\tmine\t1\t102\t80\t24\t0\n' ;;
  display-message)
    # tmux 3.8 prints an empty line, rc 0, for a pane that is gone.
    case " $* " in
      *" -t %1 "*|*" -t %7 "*|*" -t %9 "*) echo /from/pane ;;
      *) echo ;;
    esac ;;
  split-window)
    case " $* " in
      *" -t %1 "*|*" -t %7 "*|*" -t %9 "*)
        if [ -e 'DIR/split-no-space' ]; then echo "no space for a new pane" >&2; exit 1; fi
        echo %42 ;;
      *) echo "can't find pane: $4" >&2; exit 1 ;;
    esac ;;
  send-keys)
    if [ -e 'DIR/fail-send-keys' ]; then echo "send-keys failed" >&2; exit 1; fi ;;
esac
"#;

/// A temp dir whose `path/` holds the stand-in `tmux`.
fn rig() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("path");
    std::fs::create_dir_all(&path).expect("mkdir PATH dir");
    let tmux = path.join("tmux");
    let script = STAND_IN_TMUX.replace("DIR", &dir.path().display().to_string());
    std::fs::write(&tmux, script).expect("write stand-in tmux");
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).expect("chmod tmux");
    dir
}

/// Runs `atm spawn <args>` inside a fake tmux client (`TMUX` set), with
/// `TMUX_PANE` set to `caller_pane` when given.
fn spawn(dir: &Path, caller_pane: Option<&str>, args: &[&str]) -> Output {
    spawn_command(dir, caller_pane, args)
        .output()
        .expect("run atm spawn")
}

fn spawn_command(dir: &Path, caller_pane: Option<&str>, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atm"));
    cmd.arg("spawn")
        .args(args)
        .env_clear()
        .env("PATH", dir.join("path"))
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("ATM_SOCKET", dir.join("atmd.sock"))
        .env("TMUX_TMPDIR", dir)
        .env("TMUX", "/tmp/atm-spawn-test-no-real-tmux,1,0");
    if let Some(pane) = caller_pane {
        cmd.env("TMUX_PANE", pane);
    }
    cmd
}

/// The stand-in's calls, without the leading `-u`.
fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("tmux.log"))
        .unwrap_or_default()
        .lines()
        .map(|call| call.strip_prefix("-u ").unwrap_or(call).to_string())
        .collect()
}

/// The pane after `-t` in the first call to `subcommand`, compared as a
/// whole word (`%7` must not match `%70`).
fn target_of(calls: &[String], subcommand: &str) -> Option<String> {
    let call = calls
        .iter()
        .find(|c| c.split(' ').next() == Some(subcommand))?;
    let words: Vec<&str> = call.split(' ').collect();
    words
        .iter()
        .skip_while(|w| **w != "-t")
        .nth(1)
        .map(|w| (*w).to_string())
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn spawn_splits_the_callers_pane_and_prints_the_new_one() {
    let dir = rig();
    let out = spawn(dir.path(), Some("%7"), &[]);
    assert!(out.status.success(), "atm spawn: {out:?}");
    let calls = calls(dir.path());
    assert_eq!(
        target_of(&calls, "split-window").as_deref(),
        Some("%7"),
        "{calls:?}"
    );
    // The command and its Enter go to the new pane.
    let typed: Vec<_> = calls
        .iter()
        .filter(|c| c.starts_with("send-keys"))
        .collect();
    assert_eq!(typed.len(), 2, "{calls:?}");
    assert!(
        typed.iter().all(|c| c.starts_with("send-keys -t %42 ")),
        "{calls:?}"
    );
    assert_eq!(stdout(&out), "%42\n");
}

#[test]
fn spawn_prefers_an_explicit_target_over_tmux_pane() {
    let dir = rig();
    let out = spawn(dir.path(), Some("%7"), &["-t", "%9"]);
    assert!(out.status.success(), "atm spawn: {out:?}");
    let calls = calls(dir.path());
    assert_eq!(
        target_of(&calls, "split-window").as_deref(),
        Some("%9"),
        "{calls:?}"
    );
    assert_eq!(stdout(&out), "%42\n");
}

/// With no `$TMUX_PANE` (a tmux `run-shell` job on a clean server, tmux
/// 3.8), spawn still splits the first active pane, as before.
#[test]
fn spawn_without_tmux_pane_keeps_the_active_pane_fallback() {
    let dir = rig();
    let out = spawn(dir.path(), None, &[]);
    assert!(out.status.success(), "atm spawn: {out:?}");
    let calls = calls(dir.path());
    assert_eq!(
        target_of(&calls, "split-window").as_deref(),
        Some("%1"),
        "{calls:?}"
    );
    assert_eq!(stdout(&out), "%42\n");
}

#[test]
fn spawn_prints_json_with_the_canonical_harness_and_cwd() {
    let dir = rig();
    let out = spawn(
        dir.path(),
        Some("%7"),
        &["-f", "json", "-H", "cc", "-w", "/work dir"],
    );
    assert!(out.status.success(), "atm spawn: {out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("spawn JSON");
    assert_eq!(
        json,
        serde_json::json!({"pane": "%42", "harness": "claude", "cwd": "/work dir"})
    );

    // Without -w, cwd is the split pane's path.
    let out = spawn(dir.path(), Some("%7"), &["--format", "json", "-H", "pi"]);
    assert!(out.status.success(), "atm spawn: {out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("spawn JSON");
    assert_eq!(
        json,
        serde_json::json!({"pane": "%42", "harness": "pi", "cwd": "/from/pane"})
    );
}

/// `$TMUX_PANE` can name a pane that is gone: a value leaked into tmux's
/// global environment, or Codex 0.163.0's shared daemon reporting the pane
/// of the TUI that started it. Spawn fails rather than split another pane.
#[test]
fn spawn_fails_loudly_when_tmux_pane_names_no_pane() {
    let dir = rig();
    let out = spawn(dir.path(), Some("%4"), &[]);
    assert_eq!(out.status.code(), Some(1), "atm spawn: {out:?}");
    assert_eq!(stdout(&out), "");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("%4") && stderr.contains("$TMUX_PANE") && stderr.contains("pass -t"),
        "{stderr}"
    );
    assert!(stderr.contains("can't find pane: %4"), "{stderr}");
    let calls = calls(dir.path());
    assert!(
        !calls.iter().any(|c| c.starts_with("send-keys")),
        "{calls:?}"
    );
}

/// The pane id is printed as soon as the pane exists, so a caller can
/// clean up after a failure to start the agent in it.
#[test]
fn spawn_prints_the_pane_even_when_typing_the_command_fails() {
    let dir = rig();
    std::fs::write(dir.path().join("fail-send-keys"), "").expect("flag");
    let out = spawn(dir.path(), Some("%7"), &[]);
    assert_eq!(out.status.code(), Some(1), "atm spawn: {out:?}");
    assert_eq!(stdout(&out), "%42\n");
}

/// A split of `$TMUX_PANE` can fail for reasons that have nothing to do
/// with the variable, such as a pane too small to split. The "pass -t"
/// hint is only for a pane that is gone.
#[test]
fn spawn_hints_at_tmux_pane_only_when_the_pane_is_gone() {
    let dir = rig();
    std::fs::write(dir.path().join("split-no-space"), "").expect("flag");
    let out = spawn(dir.path(), Some("%7"), &[]);
    assert_eq!(out.status.code(), Some(1), "atm spawn: {out:?}");
    assert_eq!(stdout(&out), "");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no space for a new pane"), "{stderr}");
    assert!(stderr.contains("Failed to split tmux pane"), "{stderr}");
    assert!(!stderr.contains("pass -t"), "{stderr}");
}

/// A stdout that can't be written (here `/dev/full`) must not stop spawn
/// half-way: the pane already exists, so the agent still starts in it, and
/// the failure to print is reported, not a panic (exit 101).
#[test]
fn spawn_starts_the_agent_when_stdout_cannot_be_written() {
    let dir = rig();
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("open /dev/full");
    let out = spawn_command(dir.path(), Some("%7"), &[])
        .stdout(full)
        .output()
        .expect("run atm spawn");
    assert_eq!(out.status.code(), Some(1), "atm spawn: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Failed to print the new pane"), "{stderr}");
    let calls = calls(dir.path());
    let typed: Vec<_> = calls
        .iter()
        .filter(|c| c.starts_with("send-keys -t %42 "))
        .collect();
    assert_eq!(typed.len(), 2, "{calls:?}");
}
