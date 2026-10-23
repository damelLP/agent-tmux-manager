//! `atm list` against a daemon that drops the exchange.
//!
//! The daemon is a plain listener in the test's temp dir, found through
//! `ATM_SOCKET`, so nothing reaches a real atmd or tmux server.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;

use atm_protocol::DaemonMessage;

/// A daemon that dies mid-exchange closes the socket before sending
/// SessionList. `atm list` must fail, not print an empty list.
#[test]
fn test_list_fails_when_the_daemon_closes_before_the_session_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("atmd.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    let connected =
        serde_json::to_string(&DaemonMessage::connected("cli".to_string())).expect("serialize");
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
            // Read ListSessions, then drop the stream: EOF before any
            // SessionList.
            line.clear();
            let _ = reader.read_line(&mut line);
        }
    });

    let list = Command::new(env!("CARGO_BIN_EXE_atm"))
        .args(["list", "--format", "json"])
        .env("ATM_SOCKET", &socket)
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env_remove("TMUX")
        .output()
        .expect("run atm list");

    let stderr = String::from_utf8_lossy(&list.stderr);
    assert!(
        !list.status.success(),
        "atm list succeeded with stdout {:?}",
        String::from_utf8_lossy(&list.stdout)
    );
    assert!(
        stderr.contains("before sending the session list"),
        "unexpected error: {stderr}"
    );
}
