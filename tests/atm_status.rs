//! Status keeps legacy session counts but asks explicitly for the owner aggregate.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use atm_core::{SessionStatus, SessionView};
use atm_protocol::DaemonMessage;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
};

async fn status(result: Value, supported: bool, expected: &str, diagnostic: Option<&str>) {
    status_with_stdout(result, supported, expected, diagnostic, false).await;
}

async fn status_with_stdout(
    result: Value,
    supported: bool,
    expected: &str,
    diagnostic: Option<&str>,
    fail_stdout: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("rpc.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let observed = seen.clone();
    let peer = tokio::spawn(async move {
        for index in 0..2 {
            let (mut read, mut write) = loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, write) = stream.into_split();
                let mut read = BufReader::new(read);
                let mut line = String::new();
                if read.read_line(&mut line).await.unwrap() == 0 {
                    continue;
                }
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap()["type"],
                    "connect"
                );
                break (read, write);
            };
            observed.fetch_add(1, Ordering::SeqCst);
            let methods = if supported { vec!["msg.inbox"] } else { vec![] };
            write.write_all(format!("{}\n",json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"status","methods":methods})).as_bytes()).await.unwrap();
            let mut line = String::new();
            if index == 1 && !supported {
                assert_eq!(read.read_line(&mut line).await.unwrap(), 0);
                continue;
            }
            read.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let response = if index == 0 {
                assert_eq!(request["type"], "list_sessions");
                serde_json::to_value(DaemonMessage::session_list(vec![
                    SessionView {
                        status: SessionStatus::Working,
                        cost_usd: 1.25,
                        ..SessionView::default()
                    },
                    SessionView {
                        status: SessionStatus::Idle,
                        cost_usd: 0.25,
                        ..SessionView::default()
                    },
                    SessionView {
                        status: SessionStatus::AttentionNeeded,
                        cost_usd: 2.0,
                        ..SessionView::default()
                    },
                ]))
                .unwrap()
            } else {
                assert_eq!(request["method"], "msg.inbox");
                assert_eq!(request["from_pane"], "%7");
                assert_eq!(
                    request["params"],
                    json!({"for":"owner","count_only":true,"unread_only":false,"id":null,"after":null,"limit":100})
                );
                let mut reply = result.clone();
                reply["type"] = json!("response");
                reply["id"] = request["id"].clone();
                reply
            };
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(env!("CARGO_BIN_EXE_atm"))
            .arg("status")
            .stdout(if fail_stdout {
                std::process::Stdio::from(
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open("/dev/full")
                        .unwrap(),
                )
            } else {
                std::process::Stdio::piped()
            })
            .env_remove("TMUX")
            .env("TMUX_PANE", "%7")
            .env("ATM_SOCKET", socket)
            .env("ATM_STATE_DIR", dir.path().join("state"))
            .env("XDG_STATE_HOME", dir.path().join("xdg"))
            .kill_on_drop(true)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
            .wait_with_output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        out.status.code(),
        Some(if fail_stdout { 1 } else { 0 }),
        "{out:?}"
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    assert_eq!(
        seen.load(Ordering::SeqCst),
        2,
        "must query aggregate even when zero"
    );
    if let Some(diagnostic) = diagnostic {
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(diagnostic),
            "{out:?}"
        );
    } else {
        assert!(out.stderr.is_empty(), "{out:?}");
    }
    tokio::time::timeout(Duration::from_secs(30), peer)
        .await
        .unwrap()
        .unwrap();
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn status_output_failure_is_error() {
    // Dropping writeln!'s error would incorrectly turn ENOSPC into exit 0.
    status_with_stdout(
        json!({"result":{"messages":[],"unread_count":0,"next_after":null}}),
        true,
        "",
        Some("No space left on device"),
        true,
    )
    .await;
}

#[tokio::test]
async fn status_zero_preserves_existing_bytes() {
    status(
        json!({"result":{"messages":[],"unread_count":0,"next_after":null}}),
        true,
        "2↑ 1! $3.50\n",
        None,
    )
    .await;
}
#[tokio::test]
async fn status_nonzero_appends_owner_unread_from_agent_pane() {
    status(
        json!({"result":{"messages":[],"unread_count":4,"next_after":null}}),
        true,
        "2↑ 1! $3.50 4 unread\n",
        None,
    )
    .await;
}
#[tokio::test]
async fn status_old_daemon_shows_unknown_not_zero() {
    status(
        json!(null),
        false,
        "2↑ 1! $3.50 ? unread\n",
        Some("daemon is too old"),
    )
    .await;
}
#[tokio::test]
async fn status_unavailable_or_bad_result_preserves_session_summary() {
    for result in [
        json!({"error":{"code":"unavailable","message":"organisation unavailable"}}),
        json!({"result":{"messages":[],"unread_count":0,"next_after":"m2"}}),
        json!({"result":{}}),
    ] {
        status(
            result,
            true,
            "2↑ 1! $3.50 ? unread\n",
            Some("unread count unavailable"),
        )
        .await;
    }
}
#[tokio::test]
async fn status_does_not_render_owner_bodies() {
    // A valid entry must reach the aggregate guard, not fail decoding first.
    status(
        json!({"result":{"messages":[{"message_id":"m3","content":{"from":"p/lead","to":"owner","kind":"report","body":"SECRET","attachments":[],"task":null},"sent_at":"2026-11-14T00:00:00Z","status":"delivered","read_at":null,"delivery":{"phase":"delivered","reason":null,"attempt":1,"pane":"%7","session_id":null,"confirmation":null,"retry_at":null,"blocked_by":null}}],"unread_count":1,"next_after":null}}),
        true,
        "2↑ 1! $3.50 ? unread\n",
        Some("unread count unavailable"),
    )
    .await;
}
