//! `atm list` against a daemon that drops the exchange.
//!
//! The daemon is a plain listener in the test's temp dir, found through
//! `ATM_SOCKET`, so nothing reaches a real atmd or tmux server.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::process::Command;

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
    daemon::fake_daemon(dir.path(), Some(sessions));
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

#[tokio::test]
async fn json_tags_a_teammates_own_pane_but_not_its_subagent() {
    use atm_core::{SessionId, SessionView};
    let mut sessions = rows();
    sessions.extend([
        SessionView {
            id: SessionId::new("teammate"),
            parent_session_id: Some(SessionId::new("lead")),
            tmux_pane: Some("%9".into()),
            ..SessionView::default()
        },
        SessionView {
            id: SessionId::new("teammate-child"),
            parent_session_id: Some(SessionId::new("teammate")),
            tmux_pane: Some("%9".into()),
            ..SessionView::default()
        },
    ]);
    let output = list_fixture(
        sessions.clone(),
        Some("printf '%s\\t%s\\n' %7 coordinator %9 coordinator"),
        "json",
        &[],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let mut listed: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed.len(), sessions.len());
    for ((actual, original), role) in listed.iter_mut().zip(&sessions).zip([
        serde_json::json!("coordinator"),
        serde_json::Value::Null,
        serde_json::Value::Null,
        serde_json::Value::Null,
        serde_json::json!("coordinator"),
        serde_json::Value::Null,
    ]) {
        assert_eq!(
            actual.as_object_mut().unwrap().remove("role"),
            Some(role),
            "session {}",
            original.id
        );
        assert_eq!(*actual, serde_json::to_value(original).unwrap());
    }
}

#[tokio::test]
async fn json_tags_a_child_whose_parent_is_missing() {
    use atm_core::{SessionId, SessionView};
    let output = list_fixture(
        vec![SessionView {
            id: SessionId::new("orphan"),
            parent_session_id: Some(SessionId::new("missing")),
            tmux_pane: Some("%7".into()),
            ..SessionView::default()
        }],
        Some("printf '%s\\t%s\\n' %7 coordinator"),
        "json",
        &[],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let listed: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], "orphan");
    assert_eq!(listed[0]["role"], "coordinator");
}

#[tokio::test]
async fn json_does_not_tag_a_subagent_when_filters_hide_its_parent() {
    use atm_core::{SessionId, SessionStatus, SessionView};
    let sessions = vec![
        SessionView {
            id: SessionId::new("lead"),
            tmux_pane: Some("%7".into()),
            status: SessionStatus::Idle,
            project_root: Some("/lead-project".into()),
            ..SessionView::default()
        },
        SessionView {
            id: SessionId::new("child"),
            parent_session_id: Some(SessionId::new("lead")),
            tmux_pane: Some("%7".into()),
            status: SessionStatus::Working,
            project_root: Some("/child-project".into()),
            ..SessionView::default()
        },
    ];
    for filters in [["--status", "working"], ["--project", "child-project"]] {
        let output = list_fixture(
            sessions.clone(),
            Some("printf '%s\\t%s\\n' %7 coordinator"),
            "json",
            &filters,
        )
        .await;
        assert!(output.status.success(), "{filters:?}: {output:?}");
        let listed: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(listed.len(), 1, "{filters:?}");
        assert_eq!(listed[0]["id"], "child", "{filters:?}");
        assert_eq!(
            listed[0].get("role"),
            Some(&serde_json::Value::Null),
            "{filters:?}"
        );
    }
}

/// A daemon that dies mid-exchange closes the socket before sending
/// SessionList. `atm list` must fail, not print an empty list.
#[test]
fn test_list_fails_when_the_daemon_closes_before_the_session_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("atmd.sock");
    daemon::fake_daemon(dir.path(), None);

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
