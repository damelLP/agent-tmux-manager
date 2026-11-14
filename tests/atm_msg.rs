//! CLI contracts against a private scripted peer, NOT delivery acceptance.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use serde_json::{json, Value};
use std::{process::Output, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
};

struct Step {
    method: &'static str,
    params: Value,
    payload: Value,
}
fn ok(method: &'static str, params: Value, result: Value) -> Step {
    Step {
        method,
        params,
        payload: json!({"result":result}),
    }
}
async fn run(args: &[&str], pane: Option<&str>, mut steps: Vec<Step>, success: bool) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("rpc.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    for step in &mut steps {
        if step.params["project_root"] == "$cwd" {
            step.params["project_root"] = json!(dir.path());
        }
        if step.params["project_root"] == "$cwd/sub" {
            step.params["project_root"] = json!(dir.path().join("sub"));
        }
    }
    let expected_pane = pane.map(str::to_owned);
    let peer = tokio::spawn(async move {
        for step in steps {
            let (mut read, mut write) = loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, write) = stream.into_split();
                let mut read = BufReader::new(read);
                let mut hello = String::new();
                if read.read_line(&mut hello).await.unwrap() == 0 {
                    continue;
                }
                assert_eq!(
                    serde_json::from_str::<Value>(&hello).unwrap()["type"],
                    "connect"
                );
                break (read, write);
            };
            write.write_all(format!("{}\n",json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"test","methods":[step.method]})).as_bytes()).await.unwrap();
            let mut line = String::new();
            if step.method.is_empty() {
                assert_eq!(
                    read.read_line(&mut line).await.unwrap(),
                    0,
                    "old daemon must receive no request"
                );
                continue;
            }
            read.read_line(&mut line).await.unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["method"], step.method);
            assert_eq!(req["params"], step.params);
            assert_eq!(
                req.get("from_pane").and_then(Value::as_str),
                expected_pane.as_deref()
            );
            let mut reply = step.payload;
            reply["type"] = json!("response");
            reply["id"] = req["id"].clone();
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            assert_eq!(
                read.read_line(&mut line).await.unwrap(),
                0,
                "no second request"
            );
        }
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_atm"));
    command
        .args(args)
        .current_dir(dir.path())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("ATM_SOCKET", socket)
        .env("ATM_STATE_DIR", dir.path().join("state"))
        .env("XDG_STATE_HOME", dir.path().join("xdg"))
        .kill_on_drop(true);
    if let Some(pane) = pane {
        command.env("TMUX_PANE", pane);
    }
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .unwrap()
        .unwrap();
    if output.status.success() != success {
        peer.abort();
        panic!("unexpected CLI result: {output:?}");
    }
    tokio::time::timeout(Duration::from_secs(30), peer)
        .await
        .unwrap()
        .unwrap();
    output
}
fn send(to: &str, body: &str) -> Value {
    json!({"to":to,"kind":"note","body":body,"attachments":[],"task":null})
}
fn inbox() -> Value {
    json!({"for":null,"unread_only":false,"count_only":false,"id":null,"after":null,"limit":100})
}
fn agent(id: &str, pane: Option<&str>, state: &str) -> Value {
    json!({"agent_id":id,"state":state,"activity":null,"pane":pane,"session_id":null,"harness":null,"model":null,"cost_usd":null,"reason":null})
}

#[tokio::test]
async fn message_commands_send_exact_json() {
    for (args, method, params, result, text) in [
        (
            vec!["msg", "send", "--to", "p/lead", "--body", "hello"],
            "msg.send",
            send("p/lead", "hello"),
            json!({"message_id":"m3"}),
            "m3 queued\n",
        ),
        (
            vec!["msg", "send", "--to", "owner", "--body", "hello"],
            "msg.send",
            send("owner", "hello"),
            json!({"message_id":"m3"}),
            "m3 delivered to owner\n",
        ),
        (
            vec!["msg", "ack", "m1", "m2"],
            "msg.ack",
            json!({"ids":["m1","m2"]}),
            json!({"count":1}),
            "1 marked read\n",
        ),
        (
            vec!["msg", "ack", "m1"],
            "msg.ack",
            json!({"ids":["m1"]}),
            json!({"count":0}),
            "0 marked read\n",
        ),
        (
            vec!["msg", "retry", "m1", "--accept-duplicate-risk"],
            "msg.retry",
            json!({"id":"m1"}),
            json!({"message_id":"m1","status":"queued"}),
            "m1 queued (duplicate delivery possible)\n",
        ),
        (
            vec!["msg", "give-up", "m1"],
            "msg.give_up",
            json!({"id":"m1"}),
            json!({"message_id":"m1","status":"failed"}),
            "m1 failed\n",
        ),
    ] {
        let out = run(&args, None, vec![ok(method, params, result)], true).await;
        assert_eq!(String::from_utf8(out.stdout).unwrap(), text);
    }
}

#[tokio::test]
async fn send_preserves_multiline_body_and_literal_attachments_and_all_six_kinds() {
    for kind in ["note", "brief", "report", "question", "answer", "review"] {
        let body = "  first\n\n雪 \"quoted\"\n";
        let out=run(&["msg","send","--to","owner","--kind",kind,"--body",body,"--attach","not a file","--attach","../brief","--task","t1","--format","json"],Some("%7"),vec![ok("msg.send",json!({"to":"owner","kind":kind,"body":body,"attachments":["not a file","../brief"],"task":"t1"}),json!({"message_id":"m5"}))],true).await;
        assert_eq!(out.stdout, b"{\"message_id\":\"m5\"}\n");
    }
}

fn inbox_entry(body: &str) -> Value {
    json!({"message_id":"m3","content":{"from":"p/lead","to":"owner","kind":"report","body":body,"attachments":[],"task":null},"sent_at":"2026-11-14T00:00:00Z","status":"delivered","read_at":null,"delivery":{"phase":"delivered","reason":null,"attempt":1,"pane":"%7","session_id":null,"confirmation":null,"retry_at":null,"blocked_by":null}})
}

#[tokio::test]
async fn inbox_text_preserves_code_backslashes_newlines_and_tabs() {
    let body = "printf 'a\\nb'\nregex \\d+\nC:\\path\n\tindented\n";
    let out = run(
        &["inbox"],
        None,
        vec![ok(
            "msg.inbox",
            inbox(),
            json!({"messages":[inbox_entry(body)],"unread_count":1,"next_after":null}),
        )],
        true,
    )
    .await;
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("  printf 'a\\nb'\n  regex \\d+\n  C:\\path\n  \tindented\n  \n"),
        "{text}"
    );
}

#[tokio::test]
async fn inbox_is_nonmutating_preserves_json_nulls_and_escapes_text_controls() {
    let body = "one\n\n雪\t\r\u{1b}[2J\u{7}\u{7f}\u{85}";
    let entry = json!({"message_id":"m3","content":{"from":"p/lead","to":"owner","kind":"report","body":body,"attachments":["/brief\u{1b}"],"task":"t1"},"sent_at":"2026-11-14T00:00:00Z","status":"delivered","read_at":null,"delivery":{"phase":"delivered","reason":null,"attempt":1,"pane":"%7","session_id":null,"confirmation":null,"retry_at":null,"blocked_by":null}});
    let result = json!({"messages":[entry],"unread_count":2,"next_after":"m3"});
    for format in ["json", "text"] {
        let mut params = inbox();
        params["for"] = json!("owner");
        params["id"] = json!("m3");
        params["unread_only"] = json!(true);
        params["limit"] = json!(5);
        let out = run(
            &[
                "inbox", "--for", "owner", "--id", "m3", "--unread", "--limit", "5", "--format",
                format,
            ],
            None,
            vec![ok("msg.inbox", params, result.clone())],
            true,
        )
        .await;
        if format == "json" {
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stdout).unwrap(),
                result
            );
            assert_eq!(out.stdout.last(), Some(&b'\n'));
        } else {
            let text = String::from_utf8(out.stdout).unwrap();
            assert!(
                text.contains("  one\n  \n  雪\t\\r\\u{1b}[2J\\u{7}\\u{7f}\\u{85}"),
                "{text}"
            );
            for s in [
                "m3",
                "p/lead",
                "owner",
                "report",
                "/brief\\u{1b}",
                "t1",
                "delivered",
                "after",
            ] {
                assert!(text.contains(s), "{text}");
            }
            assert!(!text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t')));
        }
    }
    let mut params = inbox();
    params["after"] = json!("m3");
    run(
        &["inbox", "--after", "m3"],
        None,
        vec![ok(
            "msg.inbox",
            params,
            json!({"messages":[],"unread_count":0,"next_after":null}),
        )],
        true,
    )
    .await;
}

#[tokio::test]
async fn inbox_body_lines_cannot_forge_entry_headers() {
    let forged = "m99 from=owner to=p/x kind=brief status=delivered";
    let body = format!("first\n\n{forged}\ndelivery: forged\n");
    let out = run(&["inbox"], None, vec![ok("msg.inbox", inbox(), json!({"messages":[inbox_entry(&body), inbox_entry("real second entry")],"unread_count":2,"next_after":null}))], true).await;
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(&format!(
            "\n  first\n  \n  {forged}\n  delivery: forged\n  \nm3 from="
        )),
        "{text}"
    );
    assert!(!text
        .lines()
        .any(|line| line.starts_with("m99 ") || line == "delivery: forged"));
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("m3 from="))
            .count(),
        2
    );
}

#[tokio::test]
async fn json_preserves_additive_result_entry_and_delivery_fields() {
    let mut entry = inbox_entry("original");
    entry["extra_entry"] = json!({"nested":null});
    entry["delivery"]["extra_delivery"] = json!([1, "new"]);
    for (mut args, method, params, result) in [
        (
            vec!["msg", "send", "--to", "owner", "--body", "hi"],
            "msg.send",
            send("owner", "hi"),
            json!({"message_id":"m9","extra":1}),
        ),
        (
            vec!["msg", "ack", "m9"],
            "msg.ack",
            json!({"ids":["m9"]}),
            json!({"count":1,"extra":1}),
        ),
        (
            vec!["msg", "retry", "m9", "--accept-duplicate-risk"],
            "msg.retry",
            json!({"id":"m9"}),
            json!({"message_id":"m9","status":"queued","extra":1}),
        ),
        (
            vec!["msg", "give-up", "m9"],
            "msg.give_up",
            json!({"id":"m9"}),
            json!({"message_id":"m9","status":"failed","extra":1}),
        ),
        (
            vec!["inbox"],
            "msg.inbox",
            inbox(),
            json!({"messages":[entry],"unread_count":1,"next_after":null,"extra":1}),
        ),
    ] {
        args.extend(["--format", "json"]);
        let out = run(&args, None, vec![ok(method, params, result.clone())], true).await;
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stdout).unwrap(),
            result,
            "{method}"
        );
    }
}

#[tokio::test]
async fn json_escapes_del_and_c1_without_changing_values() {
    let controls: String = ('\u{7f}'..='\u{9f}').collect();
    let mut result =
        json!({"messages":[inbox_entry(&controls)],"unread_count":1,"next_after":null});
    result[&controls] = json!(controls);
    let out = run(
        &["inbox", "--format", "json"],
        None,
        vec![ok("msg.inbox", inbox(), result.clone())],
        true,
    )
    .await;
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        !text.chars().any(|c| ('\u{7f}'..='\u{9f}').contains(&c)),
        "{text:?}"
    );
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), result);
    assert!(text.contains("\\u007f") && text.contains("\\u009b") && text.contains("\\u009f"));
}

#[tokio::test]
async fn json_still_validates_known_result_fields() {
    for (args, method, params) in [
        (
            vec![
                "msg", "send", "--to", "owner", "--body", "hi", "--format", "json",
            ],
            "msg.send",
            send("owner", "hi"),
        ),
        (vec!["inbox", "--format", "json"], "msg.inbox", inbox()),
    ] {
        let out = run(
            &args,
            None,
            vec![ok(method, params, json!({"extra":1}))],
            false,
        )
        .await;
        assert!(out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("invalid message result from daemon"));
    }
}

#[tokio::test]
async fn bare_names_use_caller_project_or_absolute_root_without_guessing() {
    let rows = json!([
        agent("p/caller", Some("%7"), "ready"),
        agent("p/lead", Some("%8"), "ready"),
        agent("q/lead", Some("%9"), "ready")
    ]);
    run(
        &["msg", "send", "--to", "lead", "--body", "hi"],
        Some("%7"),
        vec![
            ok("agent.list", json!({"project_root":null}), rows),
            ok("msg.send", send("p/lead", "hi"), json!({"message_id":"m1"})),
        ],
        true,
    )
    .await;
    for (root, pane, expected) in [
        (Some("/q"), Some("%7"), json!("/q")),
        (Some("sub"), None, json!("$cwd/sub")),
        (None, None, json!("$cwd")),
    ] {
        let mut args = vec!["msg", "send", "--to", "lead", "--body", "hi"];
        if let Some(root) = root {
            args.extend(["--project-root", root]);
        }
        run(
            &args,
            pane,
            vec![
                ok(
                    "agent.list",
                    json!({"project_root":expected}),
                    json!([agent("q/lead", None, "starting")]),
                ),
                ok("msg.send", send("q/lead", "hi"), json!({"message_id":"m1"})),
            ],
            true,
        )
        .await;
    }
    for rows in [json!([]), json!([agent("p/caller", Some("%7"), "down")])] {
        run(
            &["msg", "send", "--to", "lead", "--body", "hi"],
            Some("%7"),
            vec![
                ok("agent.list", json!({"project_root":null}), rows),
                ok(
                    "agent.list",
                    json!({"project_root":"$cwd"}),
                    json!([agent("q/lead", None, "starting")]),
                ),
                ok("msg.send", send("q/lead", "hi"), json!({"message_id":"m1"})),
            ],
            true,
        )
        .await;
    }
}

#[tokio::test]
async fn unknown_or_ambiguous_names_and_caller_bindings_refuse_before_send() {
    for rows in [
        json!([]),
        json!([
            agent("p/lead", None, "ready"),
            agent("q/lead", None, "ready")
        ]),
    ] {
        let out = run(
            &["msg", "send", "--to", "lead", "--body", "hi"],
            None,
            vec![ok("agent.list", json!({"project_root":"$cwd"}), rows)],
            false,
        )
        .await;
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
    }
    run(
        &["msg", "send", "--to", "lead", "--body", "hi"],
        Some("%7"),
        vec![ok(
            "agent.list",
            json!({"project_root":null}),
            json!([
                agent("p/a", Some("%7"), "ready"),
                agent("q/a", Some("%7"), "ready")
            ]),
        )],
        false,
    )
    .await;
}

#[tokio::test]
async fn daemon_errors_never_retry_or_fall_back() {
    for code in ["unavailable", "refused", "conflict", "not_found", "timeout"] {
        let out = run(
            &["msg", "ack", "m1"],
            None,
            vec![Step {
                method: "msg.ack",
                params: json!({"ids":["m1"]}),
                payload: json!({"error":{"code":code,"message":"inspect inbox before retrying"}}),
            }],
            false,
        )
        .await;
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("inspect inbox before retrying"));
    }
}

#[tokio::test]
async fn old_daemon_is_rejected_before_sending_a_request() {
    let out = run(
        &["inbox"],
        None,
        vec![ok("", json!(null), json!(null))],
        false,
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("the daemon is too old; restart atmd"));
}

#[tokio::test]
async fn local_validation_and_retry_warning_need_no_connection() {
    for (args,code,needle) in [
        (vec!["msg","retry","m1"],1,"Inspect the pane first; retry can duplicate delivery. Repeat with --accept-duplicate-risk."),
        (vec!["msg","send","--to","owner","--body","x","--kind","bad"],1,"kind"),
        (vec!["msg","send","--to","system","--body","x"],1,"system"),
        (vec!["msg","send","--to","bad/name/extra","--body","x"],1,"agent"),
        (vec!["msg","ack","m01"],2,"invalid"),
        (vec!["inbox","--id","m1","--after","m2"],2,"cannot be used"),
        (vec!["msg","send","--to","owner","--body","x","--from","p/a"],2,"unexpected"),
        (vec!["msg","ack"],2,"required"),
    ] {
        let out=run(&args,None,vec![],false).await;assert_eq!(out.status.code(),Some(code),"{out:?}");assert!(String::from_utf8_lossy(&out.stderr).contains(needle),"{out:?}");
    }
}
