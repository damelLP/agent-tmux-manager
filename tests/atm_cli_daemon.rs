//! The CLI commands that read the daemon's session list: `list`, `status`,
//! `kill`, `interrupt`, `send`, `reply` and `peek`. Each one starts the
//! daemon if needed and fetches the list, and the pane commands then
//! resolve their target in it, all before any tmux call.
//!
//! Nothing here reaches a real atmd or tmux server: `atm` runs with a
//! cleared environment whose `ATM_SOCKET`, `XDG_STATE_HOME` and
//! `TMUX_TMPDIR` sit in the test's temp dir, and whose `PATH` holds no
//! `atmd` and at most a stand-in `tmux` that only logs its arguments.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output};

use atm_core::{SessionId, SessionView};
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

/// Runs `exe` with a cleared environment rooted in `dir`. `PATH` is
/// `dir/path`, which is empty unless a test puts a stand-in there.
fn atm(exe: &Path, dir: &Path, args: &[&str]) -> Output {
    let path = dir.join("path");
    std::fs::create_dir_all(&path).expect("mkdir PATH dir");
    let mut tries = 0;
    loop {
        let run = Command::new(exe)
            .args(args)
            .env_clear()
            .env("PATH", &path)
            .env("HOME", dir)
            .env("ATM_SOCKET", dir.join("atmd.sock"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("TMUX_TMPDIR", dir)
            .output();
        match run {
            // A freshly copied `exe` is busy while a child that a sibling
            // test forked mid-copy still holds the copy's write fd.
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 50 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            run => return run.expect("run atm"),
        }
    }
}

/// A stand-in daemon on `dir/atmd.sock`. It answers `connect` with
/// Connected, then answers ListSessions with `sessions`, or hangs up
/// first when `sessions` is None.
fn fake_daemon(dir: &Path, sessions: Option<Vec<SessionView>>) {
    let listener = UnixListener::bind(dir.join("atmd.sock")).expect("bind");
    let connected =
        serde_json::to_string(&DaemonMessage::connected("cli".to_string())).expect("serialize");
    let list = sessions.map(|sessions| {
        serde_json::to_string(&DaemonMessage::session_list(sessions)).expect("serialize")
    });
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
            if let Some(list) = &list {
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
    // one there, so run `atm` from a dir without it. A hard link opens no
    // writable fd; a copy (across filesystems) does, so `atm()` retries.
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).expect("mkdir bin");
    let exe = bin.join("atm");
    if std::fs::hard_link(env!("CARGO_BIN_EXE_atm"), &exe).is_err() {
        std::fs::copy(env!("CARGO_BIN_EXE_atm"), &exe).expect("copy atm");
    }

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
    fake_daemon(dir.path(), None);

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
/// look their target up in it: a known prefix reaches tmux with the
/// session's pane, and an unknown one fails before tmux.
#[test]
fn test_commands_read_the_daemons_session_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    fake_daemon(
        dir.path(),
        Some(vec![SessionView {
            id: SessionId::new("abc-session-1"),
            id_short: "abc-sess".to_string(),
            tmux_pane: Some("%5".to_string()),
            ..SessionView::default()
        }]),
    );
    let exe = Path::new(env!("CARGO_BIN_EXE_atm"));
    // A stand-in `tmux` that logs each call and succeeds.
    let tmux_log = dir.path().join("tmux.log");
    let tmux = dir.path().join("path").join("tmux");
    std::fs::create_dir_all(dir.path().join("path")).expect("mkdir PATH dir");
    std::fs::write(
        &tmux,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\n", tmux_log.display()),
    )
    .expect("write stand-in tmux");
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).expect("chmod tmux");

    let list = atm(exe, dir.path(), &["list", "--format", "json"]);
    assert!(list.status.success(), "atm list: {list:?}");
    let listed: serde_json::Value = serde_json::from_slice(&list.stdout).expect("list JSON");
    let ids: Vec<&str> = listed
        .as_array()
        .expect("JSON array")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert_eq!(ids, ["abc-session-1"]);

    let status = atm(exe, dir.path(), &["status"]);
    assert!(status.status.success(), "atm status: {status:?}");
    assert_eq!(String::from_utf8_lossy(&status.stdout), "1\u{2191} $0.00\n");

    // Each command's tmux subcommand, in `pane_commands` order.
    let subcommands = [
        "kill-pane",
        "send-keys",
        "send-keys",
        "send-keys",
        "capture-pane",
    ];
    for (args, subcommand) in pane_commands("abc").into_iter().zip(subcommands) {
        let _ = std::fs::remove_file(&tmux_log);
        let out = atm(exe, dir.path(), &args);
        assert!(out.status.success(), "atm {args:?}: {out:?}");
        let calls = std::fs::read_to_string(&tmux_log).expect("tmux was called");
        for call in calls.lines() {
            let words: Vec<&str> = call.split(' ').collect();
            assert_eq!(words.first(), Some(&subcommand), "atm {args:?}: {call:?}");
            let pane = words.iter().skip_while(|w| **w != "-t").nth(1);
            assert_eq!(pane, Some(&"%5"), "atm {args:?}: {call:?}");
        }
    }

    let _ = std::fs::remove_file(&tmux_log);
    for args in pane_commands("nope") {
        let out = atm(exe, dir.path(), &args);
        assert_fails_with(&out, &args, "Error: No session matching 'nope'");
    }
    assert!(!tmux_log.exists(), "an unknown target reached tmux");
}
