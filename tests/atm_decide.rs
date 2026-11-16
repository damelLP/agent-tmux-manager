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
fn request_args() -> Vec<&'static str> {
    vec![
        "decisions",
        "request",
        "--kind",
        "plan",
        "--title",
        "Approve this plan",
        "--option",
        "yes",
        "--option",
        "no",
        "--may-be-undone",
        "true",
        "--changes-intent",
        "true",
    ]
}
fn request_params() -> Value {
    json!({"kind":"plan","title":"Approve this plan","context":"","files":[],"options":["yes","no"],"recommended":null,"may_be_undone":true,"changes_intent":true,"deadline":null})
}
fn withdrawn(changed: bool) -> Value {
    let mut decision = list()["decisions"][0].clone();
    decision["status"] = json!("withdrawn");
    decision["closed_at"] = json!("2026-01-15T12:01:00Z");
    decision["withdrawal"] =
        json!({"decision_id":"d41","by":"project/worker","reason":"superseded"});
    json!({"decision":decision,"changed":changed})
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

// Break caught: missing/defaulted request fields, altered exact strings, or an extra post-request RPC.
#[tokio::test]
async fn decisions_request_preserves_defaults_exact_values_and_raw_json() {
    let output = exchange(
        &request_args(),
        "decision.request",
        request_params(),
        json!({"result":{"decision_id":"d41"}}),
        true,
        false,
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, b"d41 requested\n");
    let mut args = request_args();
    args.extend(["--format", "json"]);
    let raw = json!({"decision_id":"d41","future":{"keep":"é\u{009b}\u{007f}\u{001b}","null":null,"integer":9007199254740993_u64}});
    let output = exchange(
        &args,
        "decision.request",
        request_params(),
        json!({"result":raw}),
        true,
        false,
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        raw
    );
    assert!(!String::from_utf8_lossy(&output.stdout)
        .chars()
        .any(|c| matches!(c, '\u{009b}' | '\u{007f}' | '\u{001b}')));

    let args = [
        "decisions",
        "request",
        "--kind",
        "policy",
        "--title",
        "  Approve é  ",
        "--context",
        "  first\n\n雪 \"quoted\"\n",
        "--option",
        " Keep shape ",
        "--option=-value",
        "--recommended",
        " Keep shape ",
        "--attach",
        "not a file",
        "--attach",
        "../brief",
        "--may-be-undone",
        "false",
        "--changes-intent",
        "false",
        "--deadline",
        "2099-01-01T01:00:00+01:00",
    ];
    let params = json!({"kind":"policy","title":"  Approve é  ","context":"  first\n\n雪 \"quoted\"\n","files":["not a file","../brief"],"options":[" Keep shape ","-value"],"recommended":" Keep shape ","may_be_undone":false,"changes_intent":false,"deadline":"2099-01-01T00:00:00Z"});
    let output = exchange(
        &args,
        "decision.request",
        params,
        json!({"result":{"decision_id":"d41"}}),
        true,
        false,
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"d41 requested\n");
    // Cardinality is the daemon's policy, not a second CLI validator.
    let mut args = request_args();
    args.drain(8..10);
    let mut params = request_params();
    params["options"] = json!(["yes"]);
    let output = exchange(
        &args,
        "decision.request",
        params,
        json!({"error":{"code":"invalid_params","message":"options must contain 2 to 32 entries"}}),
        true,
        false,
    )
    .await;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("options must contain 2 to 32 entries")
    );
}

// Break caught: missing reason/null, invented success state or dropped additive JSON facts.
#[tokio::test]
async fn decisions_withdraw_preserves_reason_and_reports_changed_or_unchanged() {
    for changed in [true, false] {
        let raw = withdrawn(changed);
        let out = exchange(
            &["decisions", "withdraw", "d41"],
            "decision.withdraw",
            json!({"id":"d41","reason":null}),
            json!({"result":raw}),
            true,
            false,
        )
        .await;
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            out.stdout,
            if changed {
                b"d41 withdrawn\n".as_slice()
            } else {
                b"d41 withdrawn (unchanged)\n".as_slice()
            }
        );
        let mut raw = withdrawn(changed);
        raw["future"] = json!({"keep":"é\u{009b}","null":null});
        let out = exchange(
            &[
                "decisions",
                "withdraw",
                "d41",
                "--reason",
                "  superseded\n雪 \u{001b} ",
                "--format",
                "json",
            ],
            "decision.withdraw",
            json!({"id":"d41","reason":"  superseded\n雪 \u{001b} "}),
            json!({"result":raw}),
            true,
            false,
        )
        .await;
        assert!(out.status.success(), "{out:?}");
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), raw);
        assert!(!String::from_utf8_lossy(&out.stdout).contains('\u{009b}'));
    }
}

// Break caught: a new mutation hides refusals, retries, accepts malformed success, or swallows write failure.
#[tokio::test]
async fn decision_mutation_failures_never_report_success_or_retry() {
    for (args, method, params, result) in [
        (
            request_args(),
            "decision.request",
            request_params(),
            json!({"decision_id":"d41"}),
        ),
        (
            vec!["decisions", "withdraw", "d41"],
            "decision.withdraw",
            json!({"id":"d41","reason":null}),
            withdrawn(true),
        ),
    ] {
        for code in [
            "invalid_params",
            "refused",
            "not_found",
            "unavailable",
            "timeout",
        ] {
            for format in ["text", "json"] {
                let mut args = args.clone();
                args.extend(["--format", format]);
                let message = format!("server {code}");
                let out = exchange(
                    &args,
                    method,
                    params.clone(),
                    json!({"error":{"code":code,"message":message}}),
                    true,
                    false,
                )
                .await;
                assert_eq!(out.status.code(), Some(1), "{out:?}");
                assert!(out.stdout.is_empty());
                assert_eq!(
                    String::from_utf8(out.stderr).unwrap(),
                    format!("Error: {message}\n")
                );
            }
        }
        let out = exchange(&args, method, params.clone(), json!({}), false, false).await;
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("daemon is too old"));
        let malformed = if method == "decision.request" {
            vec![
                json!({}),
                json!({"decision_id":"d01"}),
                json!({"decision_id":41}),
            ]
        } else {
            vec![
                json!({}),
                json!({"decision":result["decision"]}),
                resolved(),
            ]
        };
        for result in malformed {
            let out = exchange(
                &args,
                method,
                params.clone(),
                json!({"result":result}),
                true,
                false,
            )
            .await;
            assert_eq!(out.status.code(), Some(1), "{out:?}");
            assert!(out.stdout.is_empty());
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("invalid decision result"),
                "{out:?}"
            );
        }
        for format in ["text", "json"] {
            let mut args = args.clone();
            args.extend(["--format", format]);
            let out = exchange(
                &args,
                method,
                params.clone(),
                json!({"result":result}),
                true,
                true,
            )
            .await;
            assert_eq!(out.status.code(), Some(1));
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("No space left on device"),
                "{out:?}"
            );
        }
    }
}

// Break caught: fake peers hide caller attribution, withdrawal authority, or persistence errors.
#[cfg(target_os = "linux")]
#[test]
fn real_cli_requests_and_withdrawals_use_pane_authority_and_survive_restart() {
    use delivery_support::{Confirmation, MessageRig};
    let mut rig = MessageRig::start(Confirmation::Generous);
    let seat = rig.ready_agent("worker", stand_in::StandInMode::Claude);
    let other = rig.ready_agent("other", stand_in::StandInMode::Claude);
    let mut args = request_args();
    args.extend([
        "--context",
        "  exact\n雪\n",
        "--attach",
        "not a file",
        "--attach",
        "../brief",
        "--format",
        "json",
    ]);
    let refused = rig.atm(None, &args);
    assert_eq!(refused.status.code(), Some(1));
    assert!(refused.stdout.is_empty());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("only a named agent may request"));
    let mut watch = rig.watch();
    let id = rig.atm_json(Some(&seat.pane), &args)["decision_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let decision = rig
        .call("decision.get", json!({"id":id}), Some(&seat.pane))
        .unwrap();
    assert_eq!(decision["decision"]["request"]["requester"], seat.id);
    assert_eq!(decision["decision"]["request"]["context"], "  exact\n雪\n");
    assert_eq!(
        decision["decision"]["request"]["files"],
        json!(["not a file", "../brief"])
    );
    assert_eq!(decision["decision"]["class"], "changes_intent");
    assert_eq!(decision["decision"]["status"], "pending");
    assert_eq!(
        rig.call("decision.get", json!({"id":id}), Some(&other.pane))
            .unwrap_err()
            .0,
        "refused"
    );
    assert_eq!(
        rig.atm_json(Some(&other.pane), &["decisions", "--format", "json"])["pending_count"],
        0
    );
    assert_eq!(
        rig.atm_json(Some(&seat.pane), &["decisions", "--format", "json"])["decisions"][0]["id"],
        id
    );
    let withdraw = [
        "decisions",
        "withdraw",
        &id,
        "--reason",
        "superseded",
        "--format",
        "json",
    ];
    let refused = rig.atm(Some(&other.pane), &withdraw);
    assert_eq!(refused.status.code(), Some(1));
    assert!(refused.stdout.is_empty());
    let first = rig.atm_json(Some(&seat.pane), &withdraw);
    assert_eq!(first["changed"], true);
    assert_eq!(first["decision"]["status"], "withdrawn");
    assert_eq!(first["decision"]["withdrawal"]["by"], seat.id);
    assert_eq!(first["decision"]["withdrawal"]["reason"], "superseded");
    let repeat = rig.atm_json(
        Some(&seat.pane),
        &[
            "decisions",
            "withdraw",
            &id,
            "--reason",
            "different",
            "--format",
            "json",
        ],
    );
    assert_eq!(repeat["changed"], false);
    assert_eq!(repeat["decision"], first["decision"]);
    assert_eq!(rig.atm_json(None, &withdraw)["changed"], false);
    let second = rig.atm_json(Some(&seat.pane), &args)["decision_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let owner = rig.atm_json(
        None,
        &["decisions", "withdraw", &second, "--format", "json"],
    );
    assert_eq!(owner["changed"], true);
    assert_eq!(owner["decision"]["withdrawal"]["by"], "owner");
    assert_eq!(owner["decision"]["withdrawal"]["reason"], Value::Null);
    watch.catch_up(&rig);
    assert_eq!(watch.all(|e| e["kind"] == "decision.requested").len(), 2);
    for id in [&id, &second] {
        assert_eq!(
            watch
                .all(|e| e["kind"] == "decision.requested" && format!("d{}", e["seq"]) == *id)
                .len(),
            1
        );
        assert_eq!(
            watch
                .all(|e| e["kind"] == "decision.withdrawn" && e["body"]["decision_id"] == *id)
                .len(),
            1
        );
    }
    rig.hold_turn(&seat.pane);
    rig.test.rig.restart_daemon();
    rig.finish_turn(&seat.pane);
    assert_eq!(
        rig.call("decision.get", json!({"id":id}), Some(&seat.pane))
            .unwrap()["decision"],
        first["decision"]
    );
    assert_eq!(
        rig.call("decision.get", json!({"id":second}), None)
            .unwrap()["decision"],
        owner["decision"]
    );
    assert_eq!(rig.atm_json(Some(&seat.pane), &withdraw)["changed"], false);
    assert_eq!(
        rig.atm_json(
            None,
            &["decisions", "withdraw", &second, "--format", "json"]
        )["changed"],
        false
    );
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
        vec!["decisions", "request"],
        vec!["decisions", "withdraw"],
        vec!["decisions", "withdraw", "d01"],
        vec!["decisions", "withdraw", "d41", "--all"],
        vec!["decisions", "--format", "json", "withdraw", "d41"],
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
