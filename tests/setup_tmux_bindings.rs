//! The tmux bindings `atm setup` installs (`tmux-bindings.conf`).
//!
//! `atm spawn` prints the new pane's id (#121). tmux shows a `run-shell`
//! job's stdout in view mode over the current pane once the job ends, with
//! or without `-b` (tmux 3.8, a real key press: the new agent's pane opened
//! in view-mode showing `%12`). So every spawn binding sends spawn's stdout
//! to /dev/null. A failed spawn still shows: tmux reports
//! "'atm spawn …' returned 1" in view mode. It never showed stderr.
//!
//! `atm setup` runs with a cleared environment rooted in a temp dir, so it
//! writes nothing outside it.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::{
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

/// Run real setup without any executable harness, daemon or tmux on PATH.
/// Process exit synchronizes the read; the timeout only detects hangs.
async fn setup_bindings(home: &Path) -> String {
    std::fs::create_dir_all(home.join(".pi/agent")).expect("mkdir pi dir");
    std::fs::create_dir_all(home.join("path")).expect("mkdir PATH dir");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_atm"))
        .arg("setup")
        .env_clear()
        .env("PATH", home.join("path"))
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("ATM_SOCKET", home.join("atmd.sock"))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("run atm setup");
    let status = match tokio::time::timeout(Duration::from_secs(60), child.wait()).await {
        Ok(Ok(status)) => status,
        error => {
            let _ = child.start_kill();
            let reaped = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;
            assert!(matches!(reaped, Ok(Ok(_))), "reap setup: {reaped:?}");
            panic!("setup did not finish: {error:?}");
        }
    };
    assert!(status.success(), "atm setup: {status}");
    std::fs::read_to_string(home.join(".config/atm/tmux-bindings.conf"))
        .expect("read generated bindings")
}

// Break caught: wrong key table/case/command or replacing existing bindings.
#[tokio::test]
async fn org_popup_is_uppercase_prefix_o_and_keeps_sidebar() {
    let home = tempfile::tempdir().expect("tempdir");
    let bindings = setup_bindings(home.path()).await;
    let lines: Vec<_> = bindings
        .lines()
        .filter(|line| line.starts_with("bind ") || line.starts_with("bind-key "))
        .collect();
    assert_eq!(
        lines,
        [
            "bind C-n run-shell \"atm spawn --target-pane #{pane_id} > /dev/null\"",
            "bind C-h run-shell \"atm spawn --direction left --target-pane #{pane_id} > /dev/null\"",
            "bind C-j run-shell \"atm spawn --direction below --target-pane #{pane_id} > /dev/null\"",
            "bind C-k run-shell \"atm spawn --direction above --target-pane #{pane_id} > /dev/null\"",
            "bind C-l run-shell \"atm spawn --direction right --target-pane #{pane_id} > /dev/null\"",
            "bind C-a run-shell \"atm toggle-panel\"",
            "bind C-s display-popup -E -w 35% -h 100% -x 0 \"atm\"",
            "bind O display-popup -E -w 90% -h 90% \"atm org | less -R\"",
            "bind C-t if-shell -F '#{==:#{window_name},chat}' 'last-window' 'new-window -S -n chat \"atm chat\"'",
        ]
    );
}

// Break caught: setup appends duplicate org bindings or omits them on rerun.
#[tokio::test]
async fn setup_twice_installs_org_once() {
    let home = tempfile::tempdir().expect("tempdir");
    let first = setup_bindings(home.path()).await;
    let second = setup_bindings(home.path()).await;
    assert_eq!(second, first);
    assert_eq!(
        second
            .lines()
            .filter(|line| *line == "bind O display-popup -E -w 90% -h 90% \"atm org | less -R\"")
            .count(),
        1,
        "{second}"
    );
}

// Break caught: setup appends, omits, or duplicates the chat flip binding.
#[tokio::test]
async fn setup_twice_installs_one_chat_flip_binding() {
    let home = tempfile::tempdir().expect("tempdir");
    let first = setup_bindings(home.path()).await;
    let second = setup_bindings(home.path()).await;
    assert_eq!(second, first);
    let chat: Vec<_> = second
        .lines()
        .filter(|line| line.starts_with("bind C-t "))
        .collect();
    assert_eq!(chat, ["bind C-t if-shell -F '#{==:#{window_name},chat}' 'last-window' 'new-window -S -n chat \"atm chat\"'"]);
}

#[test]
fn spawn_bindings_discard_the_printed_pane() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path();
    // Setup installs the bindings only once it detects a harness.
    std::fs::create_dir_all(home.join(".pi/agent")).expect("mkdir pi dir");
    std::fs::create_dir_all(home.join("path")).expect("mkdir PATH dir");
    let out = Command::new(env!("CARGO_BIN_EXE_atm"))
        .arg("setup")
        .env_clear()
        .env("PATH", home.join("path"))
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("ATM_SOCKET", home.join("atmd.sock"))
        .output()
        .expect("run atm setup");
    assert!(out.status.success(), "atm setup: {out:?}");

    let bindings = std::fs::read_to_string(home.join(".config/atm/tmux-bindings.conf"))
        .expect("read tmux-bindings.conf");
    let spawn_lines: Vec<&str> = bindings
        .lines()
        .filter(|line| line.starts_with("bind ") && line.contains("atm spawn"))
        .collect();
    assert_eq!(spawn_lines.len(), 5, "{bindings}");
    for line in spawn_lines {
        assert!(
            line.contains("--target-pane #{pane_id}"),
            "a run-shell job has no $TMUX_PANE of its own: {line}"
        );
        assert!(line.ends_with(" > /dev/null\""), "{line}");
    }
}
