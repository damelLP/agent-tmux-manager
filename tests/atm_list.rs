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

#[path = "support/daemon.rs"]
mod daemon;

async fn list_fixture(
    sessions: Vec<atm_core::SessionView>,
    tmux_body: Option<&str>,
    format: &str,
    filters: &[&str],
) -> std::process::Output {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    if let Some(body) = tmux_body {
        let tmux = bin.join("tmux");
        std::fs::write(
            &tmux,
            format!("#!/bin/sh\necho called >> \"$HOME/tmux.log\"\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let socket = dir.path().join("atmd.sock");
    let server = daemon::serve_sessions(&socket, sessions);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_atm"))
            .args(["list", "-f", format])
            .args(filters)
            .env_clear()
            .env("PATH", &bin)
            .env("HOME", dir.path())
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env("ATM_SOCKET", socket)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("list timed out")
    .expect("run list");
    server.join().unwrap();
    if format != "json" {
        assert!(
            !dir.path().join("tmux.log").exists(),
            "non-JSON list queried tmux"
        );
    }
    output
}

fn rows() -> Vec<atm_core::SessionView> {
    use atm_core::{SessionId, SessionView};
    vec![
        SessionView {
            id: SessionId::new("lead"),
            id_short: "lead".into(),
            tmux_pane: Some("%7".into()),
            project_root: Some("/project".into()),
            status_label: "idle".into(),
            model: "model".into(),
            ..SessionView::default()
        },
        SessionView {
            id: SessionId::new("child"),
            tmux_pane: Some("%7".into()),
            parent_session_id: Some(SessionId::new("lead")),
            ..SessionView::default()
        },
        SessionView {
            id: SessionId::new("other"),
            tmux_pane: Some("%8".into()),
            ..SessionView::default()
        },
        SessionView {
            id: SessionId::new("no-pane"),
            ..SessionView::default()
        },
    ]
}

#[tokio::test]
async fn json_adds_only_role_and_does_not_tag_subagents() {
    let rows = rows();
    let output = list_fixture(
        rows.clone(),
        Some("printf '%s\\t%s\\n' %7 coordinator %8 implementer"),
        "json",
        &[],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let mut listed: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed.len(), rows.len());
    for ((actual, original), role) in listed.iter_mut().zip(&rows).zip([
        serde_json::json!("coordinator"),
        serde_json::Value::Null,
        serde_json::Value::Null,
        serde_json::Value::Null,
    ]) {
        assert_eq!(actual.as_object_mut().unwrap().remove("role"), Some(role));
        assert_eq!(*actual, serde_json::to_value(original).unwrap());
    }
}

#[tokio::test]
async fn json_without_tmux_or_a_server_has_explicit_null_roles() {
    for tmux in [None, Some("echo no-server >&2; exit 1")] {
        let output = list_fixture(rows(), tmux, "json", &[]).await;
        assert!(output.status.success(), "{output:?}");
        let listed: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(listed.len(), 4);
        assert!(
            listed
                .iter()
                .all(|row| row.get("role") == Some(&serde_json::Value::Null)),
            "{listed:?}"
        );
    }
}

#[tokio::test]
async fn list_keeps_filters_empty_arrays_and_non_json_bytes() {
    let output = list_fixture(rows(), Some("exit 1"), "json", &["--project", "missing"]).await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"[]\n");
    let output = list_fixture(
        rows(),
        Some("printf '%s\\t%s\\n' %7 coordinator"),
        "json",
        &["--project", "project", "--status", "idle"],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let listed: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["role"], "coordinator");
    for (format, expected) in [
        ("table", "lead\tidle\t0%\tmodel\tproject\t%7\n"),
        ("ids", "lead\n"),
    ] {
        let output = list_fixture(rows(), Some("exit 1"), format, &["--project", "project"]).await;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, expected.as_bytes());
    }
}

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
