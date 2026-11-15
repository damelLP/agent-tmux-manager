//! Decision CLI arguments, exact output and private RPC, no harness/fallback.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "support/message_delivery.rs"]
mod delivery_support;
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "support/agent_lifecycle.rs"]
mod lifecycle;
#[cfg(target_os = "linux")]
#[path = "support/stand_in.rs"]
mod stand_in;
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

fn list() -> Value {
    serde_json::from_str(include_str!("fixtures/atm_decide/list.json")).unwrap()
}
fn resolved() -> Value {
    let mut decision = list()["decisions"][0].clone();
    decision["status"] = json!("resolved");
    decision["closed_at"] = json!("2026-01-15T12:01:00Z");
    decision["resolution"] = json!({"decision_id":"d41","option":"yes","by":"owner","note":"owner note","standing_rule":true});
    json!({"decision":decision,"changed":true})
}
fn command(dir: &Path, args: &[&str]) -> Command {
    use std::os::unix::fs::PermissionsExt;
    for sub in ["home", "bin", "state", "tmux", "config"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    for name in ["atmd", "tmux"] {
        let path = dir.join("bin").join(name);
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf fallback >> \"$ATM_DECIDE_FALLBACK\"\nexit 99\n",
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
        .env("ATM_TMUX_SOCKET", "decision-cli")
        .env("TMUX_PANE", "%7")
        .env("ATM_AGENT", "wrong/identity")
        .env("ATM_DECIDE_FALLBACK", dir.join("fallback"))
        .env("PATH", dir.join("bin"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}
async fn exchange(
    args: &[&str],
    method: &str,
    params: Value,
    payload: Value,
    advertised: bool,
    broken_stdout: bool,
) -> Output {
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
        write.write_all(format!("{}\n", json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"decision-cli","methods":methods})).as_bytes()).await.unwrap();
        let mut line = String::new();
        if advertised {
            read.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap(),
                json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"r1","method":method,"params":params,"from_pane":"%7"})
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
    let mut cmd = command(dir.path(), args);
    if broken_stdout {
        cmd.stdout(Stdio::from(
            std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap(),
        ));
    }
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        cmd.spawn().unwrap().wait_with_output(),
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
fn list_params() -> Value {
    json!({"statuses":null,"after":null,"limit":100,"count_only":false})
}

// Break caught: absent commands, wrong request fields, or dropped future JSON.
#[tokio::test]
async fn decisions_render_text_and_preserve_exact_json() {
    let output = exchange(
        &["decisions"],
        "decision.list",
        list_params(),
        json!({"result":list()}),
        true,
        false,
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("fixtures/atm_decide/list.txt")
    );
    let mut value = list();
    value["future"] = json!({"keep":"é\u{009b}"});
    let output = exchange(
        &["decisions", "--format", "json"],
        "decision.list",
        list_params(),
        json!({"result":value}),
        true,
        false,
    )
    .await;
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        value
    );
    assert!(!String::from_utf8(output.stdout)
        .unwrap()
        .contains('\u{009b}'));
}
#[tokio::test]
async fn decide_passes_the_answer_note_and_rule_without_requester_override() {
    let args = ["decide", "d41", "yes", "owner note", "--rule"];
    let params = json!({"id":"d41","option":"yes","note":"owner note","standing_rule":true});
    let output = exchange(
        &args,
        "decision.resolve",
        params.clone(),
        json!({"result":resolved()}),
        true,
        false,
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"d41 resolved: yes\n");
    let mut value = resolved();
    value["changed"] = json!(false);
    let output = exchange(
        &args,
        "decision.resolve",
        params,
        json!({"result":value}),
        true,
        false,
    )
    .await;
    assert_eq!(output.stdout, b"d41 resolved: yes (unchanged)\n");
    let output = exchange(
        &["decide", "d41", "yes", "--format", "json"],
        "decision.resolve",
        json!({"id":"d41","option":"yes","note":null,"standing_rule":false}),
        json!({"result":resolved()}),
        true,
        false,
    )
    .await;
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        resolved()
    );
}
#[tokio::test]
async fn decisions_filters_pagination_empty_and_control_bytes_are_safe() {
    let output = exchange(&["decisions", "--status", "resolved", "--status", "overruled", "--after", "d4", "--limit", "2"], "decision.list", json!({"statuses":["resolved","overruled"],"after":"d4","limit":2,"count_only":false}), json!({"result":{"decisions":[],"pending_count":0,"provisional_count":0,"next_after":null}}), true, false).await;
    assert_eq!(
        output.stdout,
        b"No matching decisions.\n0 pending, 0 provisional\n"
    );
    let mut value = list();
    value["decisions"][0]["request"]["title"] =
        json!("escape\u{001b}]title\u{0007}\u{009b}\u{007f}");
    value["next_after"] = json!("d41");
    let output = exchange(&["decisions", "--all"], "decision.list", json!({"statuses":["pending","taken_provisionally","resolved","overruled","expired","withdrawn"],"after":null,"limit":100,"count_only":false}), json!({"result":value}), true, false).await;
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for c in ['\u{001b}', '\u{0007}', '\u{009b}', '\u{007f}'] {
        assert!(!text.contains(c));
    }
    assert!(text.contains("next: --after d41"));
}
#[tokio::test]
async fn decisions_show_provisional_final_and_unknown_kind_facts_by_status() {
    let mut provisional = list()["decisions"][0].clone();
    provisional["id"] = json!("d42");
    provisional["status"] = json!("taken_provisionally");
    provisional["provisional"] = json!({"decision_id":"d42","option":"yes","by":"system"});
    provisional["taken_at"] = json!("2026-01-15T12:00:10Z");
    let mut final_answer = resolved()["decision"].clone();
    final_answer["id"] = json!("d43");
    final_answer["status"] = json!("overruled");
    final_answer["resolution"]["decision_id"] = json!("d43");
    final_answer["resolution"]["option"] = json!("no");
    final_answer["provisional"] = json!({"decision_id":"d43","option":"yes","by":"system"});
    final_answer["taken_at"] = json!("2026-01-15T12:00:10Z");
    let mut unknown = list()["decisions"][0].clone();
    unknown["id"] = json!("d44");
    unknown["request"]["kind"] = json!("future_kind");
    unknown["resolution"] = final_answer["resolution"].clone(); // historical stale fact; status wins
    let out = exchange(&["decisions", "--all"], "decision.list", json!({"statuses":["pending","taken_provisionally","resolved","overruled","expired","withdrawn"],"after":null,"limit":100,"count_only":false}), json!({"result":{"decisions":[provisional, final_answer, unknown],"pending_count":1,"provisional_count":1,"next_after":null}}), true, false).await;
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("d42 taken_provisionally"));
    assert!(text.contains("  provisional: yes by=system\n"));
    assert!(text.contains("d43 overruled"));
    assert!(text.contains("  answer: no by=owner rule=true\n  note: owner note\n"));
    assert!(text.contains("d44 pending from=project/worker kind=unknown"));
    assert_eq!(text.matches("  answer:").count(), 1);
    assert!(text.ends_with("1 pending, 1 provisional\n"));
}

#[tokio::test]
async fn decision_cli_refusals_old_daemon_malformed_result_and_write_errors_fail() {
    for (payload, advertised, diagnostic) in [
        (
            json!({"error":{"code":"refused","message":"only the owner may resolve a decision"}}),
            true,
            "only the owner",
        ),
        (json!({"result":{}}), true, "invalid decision result"),
        (json!({}), false, "daemon is too old"),
    ] {
        let out = exchange(
            &["decide", "d41", "yes"],
            "decision.resolve",
            json!({"id":"d41","option":"yes","note":null,"standing_rule":false}),
            payload,
            advertised,
            false,
        )
        .await;
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(diagnostic),
            "{out:?}"
        );
    }
    let out = exchange(
        &["decisions"],
        "decision.list",
        list_params(),
        json!({"result":list()}),
        true,
        true,
    )
    .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("No space left on device"));
}
// Break caught: the built CLI/daemon only record a mark, or duplicate a rule after retry/restart.
#[cfg(target_os = "linux")]
#[test]
fn real_cli_rule_command_appends_once_through_a_private_daemon() {
    use delivery_support::{about, Confirmation, MessageRig};
    let mut rig = MessageRig::start(Confirmation::Generous);
    let seat = rig.ready_agent("worker", stand_in::StandInMode::Pi);
    std::fs::write(
        rig.test.project.join(".atm/intent.md"),
        "## How we will know\n- mechanical: scripts/check\n",
    )
    .unwrap();
    let task = rig.call("task.create", json!({"title":"Private smoke","measure":{"kind":"mechanical","command":"scripts/check"}}), Some(&seat.pane)).unwrap();
    assert_eq!(task["changed"], true);
    assert_eq!(rig.call("task.create", json!({"title":"Unapproved smoke","measure":{"kind":"mechanical","command":"scripts/other"}}), Some(&seat.pane)).unwrap_err().0, "refused");
    let mut watch = rig.watch();
    let result = rig.call("decision.request", json!({"kind":"policy","title":"CLI policy","options":["yes","no"],"may_be_undone":true,"changes_intent":false}), Some(&seat.pane)).unwrap();
    let id = result["decision_id"].as_str().unwrap();
    let args = [
        "decide",
        id,
        "yes",
        "Always use this shape",
        "--rule",
        "--format",
        "json",
    ];
    assert_eq!(rig.atm_json(None, &args)["changed"], true);
    let rules = rig.test.project.join(".atm/rules.md");
    let text = std::fs::read_to_string(&rules).unwrap();
    assert!(text.contains("Always use this shape"));
    assert!(text.contains("Answer: yes"));
    assert_eq!(
        text.lines()
            .filter(|line| *line == format!("<!-- atm-rule:{id} -->"))
            .count(),
        1
    );
    assert_eq!(rig.atm_json(None, &args)["changed"], false);
    assert_eq!(std::fs::read_to_string(&rules).unwrap(), text);
    let sent = watch.wait("the answer enqueue", |e| {
        e["kind"] == "msg.sent" && e["body"]["kind"] == "answer" && e["body"]["to"] == seat.id
    });
    let message = format!("m{}", sent["seq"]);
    watch.wait("the confirmed answer", about("msg.delivered", &message));
    rig.finish_turn(&seat.pane);
    rig.test.rig.restart_daemon();
    assert_eq!(rig.atm_json(None, &args)["changed"], false);
    assert_eq!(std::fs::read_to_string(&rules).unwrap(), text);
    let inbox = rig.call("msg.inbox", json!({"for":seat.id}), None).unwrap();
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
    rig.test.rig.close();
}

#[tokio::test]
async fn decision_cli_argument_errors_do_not_contact_a_daemon() {
    for args in [
        vec!["decide"],
        vec!["decide", "d41"],
        vec!["decide", "d01", "yes"],
        vec!["decisions", "--all", "--status", "pending"],
        vec!["decisions", "--status", "future"],
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
