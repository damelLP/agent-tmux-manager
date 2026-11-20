//! The CLI boundary: capability-check the socket and preserve unavailable data.
#![allow(clippy::unwrap_used)] // Integration-test peer and assertions.

use serde_json::{json, Value};
use std::{path::Path, process::Output, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
};

const HANG_GUARD: Duration = Duration::from_secs(60);

fn command(root: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atm"));
    cmd.env_clear()
        .args(args)
        .current_dir(root)
        .env("HOME", root)
        .env("ATM_SOCKET", root.join("rpc.sock"))
        .env("ATM_STATE_DIR", root.join("state"))
        .env("TMUX_TMPDIR", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        // No installed daemon or tmux is reachable from this subprocess.
        .env("PATH", root)
        .kill_on_drop(true);
    cmd
}

async fn exchange(args: &[&str], response: Option<Value>) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("rpc.sock")).unwrap();
    let peer = tokio::spawn(async move {
        let (mut input, mut output) = loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, write) = stream.into_split();
            let mut input = BufReader::new(read);
            let mut line = String::new();
            // The CLI's liveness probe can close without a request.
            if input.read_line(&mut line).await.unwrap() > 0 {
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap()["type"],
                    "connect"
                );
                break (input, write);
            }
        };
        let mut hello = json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"usage-cli"});
        if response.is_some() {
            hello["methods"] = json!(["usage.list"]);
        }
        output
            .write_all(format!("{hello}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        if let Some(payload) = response {
            input.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["type"], "request");
            assert_eq!(request["method"], "usage.list");
            assert_eq!(request["params"], json!({}));
            assert_eq!(request["from_pane"], Value::Null);
            let mut reply = payload;
            reply["type"] = json!("response");
            reply["id"] = request["id"].clone();
            output
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
        }
        assert_eq!(
            input.read_line(&mut line).await.unwrap(),
            0,
            "only one RPC, and none for old peers"
        );
    });
    let output = tokio::time::timeout(HANG_GUARD, command(dir.path(), args).output())
        .await
        .unwrap()
        .unwrap();
    // On RED the CLI doesn't recognize `usage`: report its exit status without
    // waiting for a connection that was never attempted.
    if output.status.code() == Some(2) {
        peer.abort();
    } else {
        tokio::time::timeout(HANG_GUARD, peer)
            .await
            .unwrap()
            .unwrap();
    }
    output
}

fn weekly() -> Value {
    json!([{
        "profile":"claude", "meter":"claude", "source":"claude_code",
        "observed_at":"2026-11-19T23:42:19Z", "stale":true,
        "windows":[{"scope":null,"duration_minutes":10080,"used_percent":52.0,"resets_at":"2026-11-22T21:00:00Z"}]
    }])
}

#[tokio::test]
async fn usage_json_preserves_the_wire_shape() {
    let sample = weekly();
    let out = exchange(
        &["usage", "--format", "json"],
        Some(json!({"result": sample.clone()})),
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        sample
    );
}

#[tokio::test]
async fn usage_text_exposes_duration_reset_age_and_stale_status() {
    let out = exchange(&["usage"], Some(json!({"result":weekly()}))).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    for field in [
        "claude",
        "stale",
        "52%",
        "7d",
        "2026-11-22T21:00:00Z",
        "2026-11-19T23:42:19Z",
    ] {
        assert!(text.contains(field), "missing {field} in {text:?}");
    }
    assert!(!text.contains("0%"));
}

#[tokio::test]
async fn empty_usage_is_unavailable_not_zero() {
    let out = exchange(&["usage"], Some(json!({"result":[]}))).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("unavailable"), "{text}");
    assert!(!text.contains('%'));
    let out = exchange(&["usage", "--format", "json"], Some(json!({"result":[]}))).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        json!([])
    );
}

#[tokio::test]
async fn legacy_daemon_is_unsupported() {
    let out = exchange(&["usage", "--format", "json"], None).await;
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unsupported"));
}

#[tokio::test]
async fn usage_denied_is_not_an_empty_success() {
    let out = exchange(
        &["usage", "--format", "json"],
        Some(json!({"error":{"code":"refused","message":"only the owner may read plan usage"}})),
    )
    .await;
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("only the owner"));
}
