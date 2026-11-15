//! `atm away` and `atm back`: arguments, exact output and the one private RPC
//! each makes. A fake peer answers; no daemon, harness or fallback runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use serde_json::{json, Value};
use std::{
    path::Path,
    process::{Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
};

fn command(dir: &Path, args: &[&str]) -> Command {
    use std::os::unix::fs::PermissionsExt;
    for sub in ["home", "bin", "state", "tmux", "config"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    // Anything the CLI started instead of asking the peer would leave a mark.
    for name in ["atmd", "tmux"] {
        let path = dir.join("bin").join(name);
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf fallback >> \"$ATM_AWAY_FALLBACK\"\nexit 99\n",
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atm"));
    cmd.env_clear()
        .args(args)
        .current_dir(dir)
        .env("HOME", dir.join("home"))
        .env("SHELL", "/bin/sh")
        .env("ATM_SOCKET", dir.join("rpc.sock"))
        .env("ATM_STATE_DIR", dir.join("state"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("TMPDIR", dir)
        .env("TMUX_TMPDIR", dir.join("tmux"))
        .env("ATM_TMUX_SOCKET", "away-cli")
        .env("TMUX_PANE", "%7")
        .env("ATM_AWAY_FALLBACK", dir.join("fallback"))
        .env("PATH", dir.join("bin"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// Run the CLI against a peer that advertises `method` (or nothing), expects
/// exactly one request with empty params from pane `%7`, and answers `payload`.
async fn exchange(args: &[&str], method: &str, payload: Value, advertised: bool) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("rpc.sock")).unwrap();
    let seen = Arc::new(AtomicBool::new(false));
    let observed = seen.clone();
    let method = method.to_owned();
    let peer = tokio::spawn(async move {
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
            observed.store(true, Ordering::SeqCst);
            break (read, write);
        };
        let methods = if advertised {
            json!([method])
        } else {
            json!([])
        };
        write.write_all(format!("{}\n", json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"away-cli","methods":methods})).as_bytes()).await.unwrap();
        let mut line = String::new();
        if advertised {
            read.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap(),
                json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"r1","method":method,"params":{},"from_pane":"%7"})
            );
            let mut response = json!({"type":"response","id":"r1"});
            response
                .as_object_mut()
                .unwrap()
                .extend(payload.as_object().unwrap().clone());
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
        }
        assert_eq!(read.read_line(&mut line).await.unwrap(), 0, "one RPC only");
    });
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        command(dir.path(), args)
            .spawn()
            .unwrap()
            .wait_with_output(),
    )
    .await
    .unwrap()
    .unwrap();
    if !seen.load(Ordering::SeqCst) {
        peer.abort();
        panic!(
            "CLI never connected: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    tokio::time::timeout(Duration::from_secs(60), peer)
        .await
        .unwrap()
        .unwrap();
    assert!(!dir.path().join("fallback").exists());
    output
}

async fn text(args: &[&str], method: &str, result: Value) -> String {
    let out = exchange(args, method, json!({ "result": result }), true).await;
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

const RULES: &str = "Details: the agent that asked decides. Changes to the intent that can be undone: taken provisionally on the recommended option. Anything that cannot be undone or reaches outside, and any action, hire or budget: waits for you.\n";

#[tokio::test]
async fn away_prints_the_time_the_rules_the_caps_and_what_is_pending() {
    let away = |caps: Value, pending: u64| json!({"changed":true,"away_since":"2026-11-16T08:02:11.500Z","stacking_caps":caps,"pending_count":pending});
    assert_eq!(
        text(&["away"], "owner.away", away(json!({"atm":1}), 3)).await,
        format!("Away since 2026-11-16T08:02:11Z.\n{RULES}Stacking cap: atm 1\n3 pending decisions are now handled by these rules: atm decisions\n")
    );
    assert_eq!(
        text(&["away"], "owner.away", away(json!({"atm":1,"other":3}), 1)).await,
        format!("Away since 2026-11-16T08:02:11Z.\n{RULES}Stacking cap: atm 1, other 3\n1 pending decision is now handled by these rules: atm decisions\n")
    );
    // No project and nothing pending: neither line is printed.
    assert_eq!(
        text(&["away"], "owner.away", away(json!({}), 0)).await,
        format!("Away since 2026-11-16T08:02:11Z.\n{RULES}")
    );
    // A repeat says so, and nothing else.
    let mut repeat = away(json!({"atm":1}), 2);
    repeat["changed"] = json!(false);
    assert_eq!(
        text(&["away"], "owner.away", repeat).await,
        "Already away since 2026-11-16T08:02:11Z.\n"
    );
}

#[tokio::test]
async fn back_prints_the_time_the_counts_and_the_digest_placeholder() {
    let back = |since: Value, provisional: u64, pending: u64| json!({"changed":true,"away_since":since,"provisional_count":provisional,"pending_count":pending});
    assert_eq!(
        text(&["back"], "owner.back", back(json!("2026-11-16T08:02:11Z"), 4, 2)).await,
        "Back. You were away since 2026-11-16T08:02:11Z.\n4 decisions taken provisionally wait for your answer; 2 are pending: atm decisions\nDigest: not built yet (unit A2).\n"
    );
    assert_eq!(
        text(&["back"], "owner.back", back(json!("2026-11-16T08:02:11Z"), 1, 1)).await,
        "Back. You were away since 2026-11-16T08:02:11Z.\n1 decision taken provisionally waits for your answer; 1 is pending: atm decisions\nDigest: not built yet (unit A2).\n"
    );
    // The switch was set by another call in between: the time is not known.
    assert_eq!(
        text(&["back"], "owner.back", back(Value::Null, 0, 0)).await,
        "Back.\n0 decisions taken provisionally wait for your answer; 0 are pending: atm decisions\nDigest: not built yet (unit A2).\n"
    );
    let not_away =
        json!({"changed":false,"away_since":null,"provisional_count":4,"pending_count":2});
    assert_eq!(text(&["back"], "owner.back", not_away).await, "Not away.\n");
}

#[tokio::test]
async fn json_format_passes_the_daemons_result_through_with_later_fields() {
    for (args, method, result) in [
        (
            ["away", "--format", "json"],
            "owner.away",
            json!({"changed":true,"away_since":"2026-11-16T08:02:11Z","stacking_caps":{"atm":1},"pending_count":3,"later":{"field":true}}),
        ),
        (
            ["back", "--format", "json"],
            "owner.back",
            json!({"changed":true,"away_since":"2026-11-16T08:02:11Z","provisional_count":4,"pending_count":2,"digest":["later"]}),
        ),
    ] {
        let out = text(&args, method, result.clone()).await;
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap(),
            result,
            "{method}"
        );
        assert!(out.ends_with('\n') && out.lines().count() == 1, "{out:?}");
    }
}

#[tokio::test]
async fn refusals_an_old_daemon_and_a_malformed_result_fail_without_output() {
    for (args, method) in [(["away"], "owner.away"), (["back"], "owner.back")] {
        for (payload, advertised, diagnostic) in [
            (
                json!({"error":{"code":"refused","message":"only the owner may set away or back"}}),
                true,
                "only the owner may set away or back",
            ),
            (
                json!({"error":{"code":"refused","message":"/project/.atm/away.toml: stacking_cap must be at least 1"}}),
                true,
                "/project/.atm/away.toml: stacking_cap must be at least 1",
            ),
            (
                json!({"result":{"changed":true}}),
                true,
                "invalid away result",
            ),
            (json!({}), false, "daemon is too old"),
        ] {
            let out = exchange(&args, method, payload, advertised).await;
            assert!(!out.status.success(), "{method}: {out:?}");
            assert!(out.stdout.is_empty(), "{method}: {out:?}");
            assert!(
                String::from_utf8_lossy(&out.stderr).contains(diagnostic),
                "{method}: {out:?}"
            );
        }
    }
}

#[tokio::test]
async fn argument_errors_do_not_contact_a_daemon() {
    for args in [
        vec!["away", "now"],
        vec!["away", "--cap", "2"],
        vec!["away", "--format", "yaml"],
        vec!["back", "d41"],
        vec!["back", "--format"],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let out = command(dir.path(), &args)
            .spawn()
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(!dir.path().join("fallback").exists());
    }
}
