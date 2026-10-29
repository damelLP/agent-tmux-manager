//! The CLI commands that read the daemon's session list: `list`, `status`,
//! `kill`, `interrupt`, `send`, `reply` and `peek`. Each one starts the
//! daemon if needed and fetches the list, and the pane commands then
//! resolve their target in it, all before any tmux call.
//!
//! Nothing here reaches a real atmd or tmux server: `atm` runs with a
//! cleared environment whose `ATM_SOCKET`, `XDG_STATE_HOME` and
//! `TMUX_TMPDIR` sit in the test's temp dir, and whose `PATH` is an empty
//! dir, so neither `atmd` nor `tmux` can be found there.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output};

use atm_protocol::DaemonMessage;

/// The five commands that act on one agent's pane.
fn pane_commands(target: &str) -> Vec<Vec<&str>> {
    vec![
        vec!["kill", target],
        vec!["interrupt", target],
        vec!["send", target, "hello"],
        vec!["reply", target],
        vec!["peek", target],
    ]
}

/// Every command that reads the session list. `%0` is a pane ID, which
/// the pane commands use as is, with no lookup.
fn all_commands() -> Vec<Vec<&'static str>> {
    let mut commands = vec![vec!["list", "--format", "json"], vec!["status"]];
    commands.extend(pane_commands("%0"));
    commands
}

/// Runs `exe` with a cleared environment rooted in `dir`.
fn atm(exe: &Path, dir: &Path, args: &[&str]) -> Output {
    let empty = dir.join("empty-path");
    std::fs::create_dir_all(&empty).expect("mkdir empty PATH dir");
    Command::new(exe)
        .args(args)
        .env_clear()
        .env("PATH", &empty)
        .env("HOME", dir)
        .env("ATM_SOCKET", dir.join("atmd.sock"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("TMUX_TMPDIR", dir)
        .output()
        .expect("run atm")
}

/// A stand-in daemon on `dir/atmd.sock`. It answers `connect` with
/// Connected, then answers ListSessions with an empty SessionList, or
/// hangs up first when `send_list` is false.
fn fake_daemon(dir: &Path, send_list: bool) {
    let listener = UnixListener::bind(dir.join("atmd.sock")).expect("bind");
    let connected =
        serde_json::to_string(&DaemonMessage::connected("cli".to_string())).expect("serialize");
    let list = serde_json::to_string(&DaemonMessage::session_list(Vec::new())).expect("serialize");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept");
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            // `atm` first checks that something listens, then hangs up
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            (&stream)
                .write_all(format!("{connected}\n").as_bytes())
                .expect("reply Connected");
            // Read ListSessions
            line.clear();
            let _ = reader.read_line(&mut line);
            if send_list {
                let _ = (&stream).write_all(format!("{list}\n").as_bytes());
                // Read Disconnect
                line.clear();
                let _ = reader.read_line(&mut line);
            }
        }
    });
}

fn assert_fails_with(out: &Output, args: &[&str], expected: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "atm {args:?}: stdout {:?}, stderr {stderr:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(stderr.trim_end(), expected, "atm {args:?}");
}

/// No daemon listens and no `atmd` can be found, so every command fails
/// at the auto-start, before it asks for the session list.
#[test]
fn test_commands_fail_when_the_daemon_cannot_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The auto-start looks for `atmd` beside `atm` first, and cargo builds
    // one there, so run a copy from a dir without it.
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).expect("mkdir bin");
    let exe = bin.join("atm");
    std::fs::copy(env!("CARGO_BIN_EXE_atm"), &exe).expect("copy atm");

    for args in all_commands() {
        let out = atm(&exe, dir.path(), &args);
        // `ensure_daemon_running`'s own message already starts with
        // "Failed to start daemon", and the CLI adds the same words.
        assert_fails_with(
            &out,
            &args,
            "Error: Failed to start daemon: Failed to start daemon: \
             No such file or directory (os error 2)",
        );
    }
}

/// A daemon that hangs up before sending SessionList fails every command,
/// including the pane commands whose `%0` target needs no lookup.
#[test]
fn test_commands_fail_when_the_daemon_closes_before_the_session_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    fake_daemon(dir.path(), false);

    for args in all_commands() {
        let out = atm(Path::new(env!("CARGO_BIN_EXE_atm")), dir.path(), &args);
        assert_fails_with(
            &out,
            &args,
            "Error: Daemon closed the connection before sending the session list",
        );
    }
}

/// `list` and `status` print the daemon's list, and the pane commands
/// look their target up in it.
#[test]
fn test_commands_read_the_daemons_session_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    fake_daemon(dir.path(), true);
    let exe = Path::new(env!("CARGO_BIN_EXE_atm"));

    // `status` is checked up to its cost: an empty list's total prints as
    // `$-0.00` today, which isn't what this file pins.
    for (args, expected) in [
        (vec!["list", "--format", "json"], "[]"),
        (vec!["status"], "0\u{2191} $"),
    ] {
        let out = atm(exe, dir.path(), &args);
        assert!(
            out.status.success(),
            "atm {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.trim_end().starts_with(expected),
            "atm {args:?}: {stdout:?}"
        );
    }

    for args in pane_commands("nope") {
        let out = atm(exe, dir.path(), &args);
        assert_fails_with(&out, &args, "Error: No session matching 'nope'");
    }
}
