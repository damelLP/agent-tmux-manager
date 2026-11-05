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

use std::process::Command;

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
