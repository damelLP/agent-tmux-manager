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
    status_aggregates(
        (result, supported),
        (json!({"result":{"decisions":[],"pending_count":0,"provisional_count":0,"next_after":null}}), supported),
        expected, diagnostic, fail_stdout,
    ).await;
}

async fn status_aggregates(
    unread: (Value, bool),
    decisions: (Value, bool),
    expected: &str,
    diagnostic: Option<&str>,
    fail_stdout: bool,
) -> std::process::Output {
    let (result, supported) = unread;
    let (decision_result, decision_supported) = decisions;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("rpc.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let observed = seen.clone();
    let peer = tokio::spawn(async move {
        for index in 0..3 {
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
            let mut methods = Vec::new();
            if supported {
                methods.push("msg.inbox");
            }
            if decision_supported {
                methods.push("decision.list");
            }
            write.write_all(format!("{}\n",json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"status","methods":methods})).as_bytes()).await.unwrap();
            let mut line = String::new();
            if (index == 1 && !supported) || (index == 2 && !decision_supported) {
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
                assert_eq!(request["from_pane"], "%7");
                let mut reply = if index == 1 {
                    assert_eq!(request["method"], "msg.inbox");
                    assert_eq!(
                        request["params"],
                        json!({"for":"owner","count_only":true,"unread_only":false,"id":null,"after":null,"limit":100})
                    );
                    result.clone()
                } else {
                    assert_eq!(request["method"], "decision.list");
                    assert_eq!(
                        request["params"],
                        json!({"statuses":null,"after":null,"limit":100,"count_only":true})
                    );
                    decision_result.clone()
                };
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
        3,
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
    out
}
#[tokio::test]
async fn status_decision_counts_and_failures_are_independent_of_unread() {
    let unread = json!({"result":{"messages":[],"unread_count":4,"next_after":null}});
    status_aggregates((unread.clone(), true), (json!({"result":{"decisions":[],"pending_count":3,"provisional_count":2,"next_after":null}}), true), "2↑ 1! $3.50 4 unread 3 decisions\n", None, false).await;
    status_aggregates((unread.clone(), true), (json!({"result":{"decisions":[],"pending_count":0,"provisional_count":2,"next_after":null}}), true), "2↑ 1! $3.50 4 unread\n", None, false).await;
    for bad in [
        json!({"result":{}}),
        json!({"result":{"decisions":[],"pending_count":0,"provisional_count":0,"next_after":"d1"}}),
        json!({"error":{"code":"unavailable","message":"org unavailable"}}),
    ] {
        status_aggregates(
            (unread.clone(), true),
            (bad, true),
            "2↑ 1! $3.50 4 unread ? decisions\n",
            Some("decision count unavailable"),
            false,
        )
        .await;
    }
    status_aggregates(
        (unread.clone(), true),
        (Value::Null, false),
        "2↑ 1! $3.50 4 unread ? decisions\n",
        Some("daemon is too old"),
        false,
    )
    .await;
    status_aggregates((json!({"error":{"code":"unavailable","message":"messages unavailable"}}), true), (json!({"result":{"decisions":[],"pending_count":2,"provisional_count":0,"next_after":null}}), true), "2↑ 1! $3.50 ? unread 2 decisions\n", Some("unread count unavailable"), false).await;
}

// Break caught: a status line that hides a forgotten `atm back`, shows the
// mark while the owner is present, or shows it for a daemon that cannot say.
#[tokio::test]
async fn status_shows_away_only_while_the_daemon_says_so() {
    let unread = json!({"result":{"messages":[],"unread_count":4,"next_after":null}});
    let decisions = |pending: u64, away: Option<bool>| {
        let mut result =
            json!({"decisions":[],"pending_count":pending,"provisional_count":2,"next_after":null});
        if let Some(away) = away {
            result["away"] = json!(away);
        }
        (json!({ "result": result }), true)
    };
    for (pending, away, expected) in [
        (3, Some(true), "2↑ 1! $3.50 4 unread 3 decisions away\n"),
        (0, Some(true), "2↑ 1! $3.50 4 unread away\n"),
        (3, Some(false), "2↑ 1! $3.50 4 unread 3 decisions\n"),
        // A daemon from before the away switch sends no such field.
        (3, None, "2↑ 1! $3.50 4 unread 3 decisions\n"),
        (0, None, "2↑ 1! $3.50 4 unread\n"),
    ] {
        status_aggregates(
            (unread.clone(), true),
            decisions(pending, away),
            expected,
            None,
            false,
        )
        .await;
    }
    // The mark is not guessed when the count cannot be read.
    status_aggregates(
        (unread, true),
        (
            json!({"error":{"code":"unavailable","message":"org unavailable"}}),
            true,
        ),
        "2↑ 1! $3.50 4 unread ? decisions\n",
        Some("decision count unavailable"),
        false,
    )
    .await;
}

#[tokio::test]
async fn status_rejects_decision_rows_without_printing_private_context() {
    let mut rows: Value =
        serde_json::from_str(include_str!("fixtures/atm_decide/list.json")).unwrap();
    rows["decisions"][0]["request"]["context"] = json!("SECRET DECISION");
    let out = status_aggregates(
        (
            json!({"result":{"messages":[],"unread_count":0,"next_after":null}}),
            true,
        ),
        (json!({"result":rows}), true),
        "2↑ 1! $3.50 ? decisions\n",
        Some("decision count unavailable"),
        false,
    )
    .await;
    assert!(!String::from_utf8_lossy(&out.stderr).contains("SECRET DECISION"));
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
        "2↑ 1! $3.50 ? unread ? decisions\n",
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
