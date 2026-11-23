//! CLI contracts on a private scripted socket, plus the real sealed daemon path.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[allow(dead_code)]
#[path = "support/agent_lifecycle.rs"]
mod lifecycle;
#[path = "support/stand_in.rs"]
mod stand_in;
#[allow(dead_code)]
#[path = "support/message_delivery.rs"]
mod support;
#[allow(dead_code)]
#[path = "support/task_cli.rs"]
mod task_cli;

use serde_json::{json, Value};
use std::{path::Path, process::Output};
use support::{
    about, reference, run_cli_script, run_cli_script_with_methods, CliStep, Confirmation,
    MessageRig,
};

fn ok(method: &'static str, params: Value, result: Value) -> CliStep {
    CliStep::Response {
        method,
        params,
        payload: json!({"result": result}),
    }
}

async fn run_in(dir: &Path, args: &[&str], pane: Option<&str>, steps: Vec<CliStep>) -> Output {
    run_cli_script(dir, task_cli::command(dir, args, pane), pane, steps).await
}

async fn run(args: &[&str], pane: Option<&str>, steps: Vec<CliStep>) -> Output {
    let dir = tempfile::tempdir().unwrap();
    run_in(dir.path(), args, pane, steps).await
}

fn text(out: Output) -> String {
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn failure(out: Output, reason: &str) {
    assert!(!out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(reason),
        "{out:?}"
    );
}

fn raw_json(out: Output, expected: &Value) {
    let stdout = text(out);
    assert_eq!(serde_json::from_str::<Value>(&stdout).unwrap(), *expected);
    assert!(stdout.ends_with('\n'));
    assert_eq!(stdout.lines().count(), 1);
    assert!(!stdout.trim_end_matches('\n').chars().any(char::is_control));
}

fn project_row() -> Value {
    json!({
        "id":"alpha", "kind":"project", "project":"alpha", "section":"channel",
        "members":[{"address":"owner","state":null},{"address":"alpha/build-2","state":"ready"},{"address":"alpha/lead","state":"ready"}],
        "former":[], "lead":"alpha/lead", "member":true, "writable":true,
        "last_post":"m500", "last_activity":"2026-11-20T21:42:00Z", "activity_seq":500,
        "unread":1, "seen_through":null
    })
}

fn direct_row(id: &str, a: &str, b: &str) -> Value {
    json!({
        "id":id, "kind":"direct", "project":null, "section":"direct",
        "members":[{"address":a,"state":"ready"},{"address":b,"state":"ready"}],
        "former":[], "lead":null, "member":false, "writable":false,
        "last_post":"m490", "last_activity":"2026-11-20T21:40:00Z", "activity_seq":490,
        "unread":0, "seen_through":null
    })
}

fn list_params(all: bool) -> Value {
    json!({"all":all})
}

fn list_page(rows: Vec<Value>) -> Value {
    json!({"seq":502,"channels":rows})
}

fn history_params() -> Value {
    json!({"channel":"alpha","thread":null,"before":null,"after":null,"limit":50})
}

fn history_row() -> Value {
    json!({
        "id":"m500", "channel":"alpha", "thread":null, "author":"alpha/lead",
        "session_id":null, "session_started":null, "kind":"report", "body":"ready",
        "attachments":["brief.md"], "task":"t7", "mentions":[], "posted_at":"2026-11-20T21:42:00Z",
        "replies":2, "last_reply_at":"2026-11-20T21:43:00Z", "routing":"none", "woke":[],
        "legacy":false, "decision":null
    })
}

fn history_page(channel: &str, posts: Vec<Value>, more: bool) -> Value {
    json!({"seq":510,"channel":channel,"posts":posts,"more":more})
}

// Break: all is not forwarded, or the CLI filters/sorts the daemon's rows.
#[tokio::test]
async fn channels_sends_scope_once_and_renders_rows() {
    let empty = run(
        &["channels"],
        Some("%7"),
        vec![ok("channel.list", list_params(false), list_page(vec![]))],
    )
    .await;
    assert_eq!(text(empty), "no channels\n");
    let all = run(
        &["channels", "--all"],
        Some("%7"),
        vec![ok(
            "channel.list",
            list_params(true),
            list_page(vec![
                project_row(),
                direct_row("alpha/lead+beta/lead", "alpha/lead", "beta/lead"),
            ]),
        )],
    )
    .await;
    assert_eq!(
        text(all),
        concat!(
            "alpha section=channel writable=true unread=1 last=m500\n",
            "alpha/lead+beta/lead section=direct writable=false unread=0 last=m490\n"
        )
    );
}

// Break: wire bounds/ids are dropped, a page is resorted, or the CLI fetches more itself.
#[tokio::test]
async fn history_preserves_page_order_and_forwards_bounds() {
    let page = run(
        &["history", "alpha"],
        None,
        vec![ok(
            "channel.history",
            history_params(),
            history_page("alpha", vec![history_row()], true),
        )],
    )
    .await;
    assert_eq!(
        text(page),
        concat!(
            "channel: alpha\n",
            "m500 from=alpha/lead kind=report at=2026-11-20T21:42:00+00:00 thread=- replies=2\n",
            "task: t7\nattach: brief.md\n  ready\nmore: true\n"
        )
    );
    let channel = "alpha/lead+owner";
    let mut first = history_row();
    first["id"] = json!("m490");
    first["channel"] = json!(channel);
    first["thread"] = json!("m480");
    first["task"] = Value::Null;
    first["body"] = json!("line\twith\\literal\n\u{1b}[31m");
    first["attachments"] = json!([]);
    let mut second = history_row();
    second["channel"] = json!(channel);
    second["thread"] = json!("m480");
    let thread = run(
        &[
            "history", channel, "--thread", "m501", "--before", "m560", "--after", "m480",
            "--limit", "2",
        ],
        Some("%7"),
        vec![ok(
            "channel.history",
            json!({"channel":channel,"thread":"m501","before":"m560","after":"m480","limit":2}),
            history_page(channel, vec![first, second], true),
        )],
    )
    .await;
    assert_eq!(
        text(thread),
        concat!(
            "channel: alpha/lead+owner\n",
            "m490 from=alpha/lead kind=report at=2026-11-20T21:42:00+00:00 thread=m480 replies=2\n",
            "  line\twith\\literal\n  \\u{1b}[31m\n",
            "m500 from=alpha/lead kind=report at=2026-11-20T21:42:00+00:00 thread=m480 replies=2\n",
            "task: t7\nattach: brief.md\n  ready\nmore: true\n"
        )
    );
    let empty = run(
        &["history", "alpha"],
        None,
        vec![ok(
            "channel.history",
            history_params(),
            history_page("alpha", vec![], false),
        )],
    )
    .await;
    assert_eq!(text(empty), "channel: alpha\nno posts\n");
}

// Break: serialize a typed projection instead of the full result or leak raw controls.
#[tokio::test]
async fn channel_read_json_preserves_additive_fields() {
    let mut channel = project_row();
    channel["future"] = json!({"null":null,"controls":"\u{7f}\u{85}"});
    let list = list_page(vec![channel]);
    raw_json(
        run(
            &["channels", "--format", "json"],
            None,
            vec![ok("channel.list", list_params(false), list.clone())],
        )
        .await,
        &list,
    );

    let mut post = history_row();
    post["future"] = json!({"null":null,"controls":"\u{7f}\u{85}"});
    let page = history_page("alpha", vec![post], false);
    raw_json(
        run(
            &["history", "alpha", "--format", "json"],
            None,
            vec![ok("channel.history", history_params(), page.clone())],
        )
        .await,
        &page,
    );
}

#[tokio::test]
async fn post_mutation_cli_uses_one_rpc_with_no_create_fallback() {
    let reply = json!({"seq":551,"post_id":"m500"});
    raw_json(
        run(
            &[
                "post", "--edit", "m500", "--body", "fixed", "--format", "json",
            ],
            Some("%7"),
            vec![ok(
                "channel.edit",
                json!({"post":"m500","body":"fixed"}),
                reply.clone(),
            )],
        )
        .await,
        &reply,
    );
    assert_eq!(
        text(
            run(
                &["post", "--remove", "m500"],
                None,
                vec![ok("channel.remove", json!({"post":"m500"}), reply)]
            )
            .await
        ),
        "m500 removed\n"
    );
    failure(
        run(
            &["post", "--remove", "m500"],
            None,
            vec![CliStep::Response {
                method: "channel.remove",
                params: json!({"post":"m500"}),
                payload: json!({"error":{"code":"unknown_method","message":"not supported"}}),
            }],
        )
        .await,
        "not supported",
    );
}

#[tokio::test]
async fn post_mutation_flags_are_exclusive_before_io() {
    for (args, reason) in [
        (&["post", "--edit", "m500"][..], "--body"),
        (
            &["post", "--remove", "m500", "--body", "no"],
            "cannot be used with",
        ),
        (
            &["post", "--edit", "m500", "--remove", "m501", "--body", "no"],
            "cannot be used with",
        ),
        (
            &["post", "--remove", "m500", "--kind", "note"],
            "cannot be used with",
        ),
        (
            &[
                "post",
                "--edit",
                "m500",
                "--body",
                "no",
                "--channel",
                "alpha",
            ],
            "cannot be used with",
        ),
        (
            &["post", "--edit", "m500", "--body", "no", "--tag", "lead"],
            "cannot be used with",
        ),
        (
            &["post", "--remove", "m500", "--attach", "plan.md"],
            "cannot be used with",
        ),
    ] {
        failure(run(args, None, vec![]).await, reason);
    }
}

#[tokio::test]
async fn history_renders_post_mutation_without_control_text() {
    let mut edited = history_row();
    edited["edited"] = json!(true);
    edited["body"] = json!("fixed\u{1b}");
    let mut removed = history_row();
    removed["id"] = json!("m499");
    removed["removed"] = json!(true);
    removed["body"] = json!("removed");
    removed["attachments"] = json!([]);
    removed["task"] = Value::Null;
    let output = text(
        run(
            &["history", "alpha"],
            None,
            vec![ok(
                "channel.history",
                history_params(),
                history_page("alpha", vec![edited, removed], false),
            )],
        )
        .await,
    );
    assert!(output.contains("replies=2 edited\n"), "{output}");
    assert!(output.contains("  fixed\\u{1b}\n"), "{output}");
    assert_eq!(output.matches("removed").count(), 1, "{output}");
}

#[tokio::test]
async fn reaction_cli_uses_one_rpc_and_prints_the_canonical_id() {
    let params = json!({"post":"m510","emoji":"👀"});
    let reply = json!({"seq":532,"post_id":"m500"});
    raw_json(
        run(
            &["react", "m510", "👀", "--format", "json"],
            Some("%7"),
            vec![ok("channel.react", params.clone(), reply.clone())],
        )
        .await,
        &reply,
    );
    assert_eq!(
        text(
            run(
                &["react", "m510", "👀"],
                Some("%7"),
                vec![ok("channel.react", params.clone(), reply)]
            )
            .await
        ),
        "m500 reaction toggled\n"
    );
    failure(
        run(
            &["react", "m510", "👀"],
            None,
            vec![CliStep::Response {
                method: "channel.react",
                params: params.clone(),
                payload: json!({"error":{"code":"unknown_method","message":"not supported"}}),
            }],
        )
        .await,
        "not supported",
    );
    failure(
        run(
            &["react", "m510", "👀"],
            None,
            vec![CliStep::Response {
                method: "channel.react",
                params,
                payload: json!({"error":{"code":"timeout","message":"uncertain"}}),
            }],
        )
        .await,
        "check `atm history",
    );
    failure(
        run(&["react", "m510", "👎"], None, vec![]).await,
        "invalid value",
    );
}

#[tokio::test]
async fn history_prints_reaction_counts_and_your_selection() {
    let mut row = history_row();
    row["reactions"] =
        json!([{"emoji":"👍","count":2,"mine":true},{"emoji":"👀","count":1,"mine":false}]);
    let output = text(
        run(
            &["history", "alpha"],
            None,
            vec![ok(
                "channel.history",
                history_params(),
                history_page("alpha", vec![row], false),
            )],
        )
        .await,
    );
    assert!(output.contains("reactions: 👍=2 (you) 👀=1\n"), "{output}");
}

// Break: invalid CLI input reaches the wire (including tolerant ChannelId::Other).
#[tokio::test]
async fn history_rejects_bad_values_without_rpc() {
    for (args, wrong) in [
        (vec!["history", "owner+alpha/lead"], "owner+alpha/lead"),
        (vec!["history", "alpha", "--thread", "m01"], "m01"),
        (vec!["history", "alpha", "--after", "garbage"], "garbage"),
        (vec!["history", "alpha", "--limit", "0"], "--limit"),
        (vec!["history", "alpha", "--limit", "201"], "--limit"),
    ] {
        let out = run(&args, None, vec![]).await;
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("unrecognized subcommand"),
            "{out:?}"
        );
        failure(out, wrong);
    }
}

// Break: a CLI/client assumption about channel IDs or rows disagrees with actual atmd.
#[test]
fn real_cli_reads_a_legacy_dm_in_a_private_rig() {
    let rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", stand_in::StandInMode::Pi);
    let id = rig.send(Some(&lead.pane), "owner", "legacy channel read");
    let channel = format!("{}+owner", lead.id);
    let channels = rig.atm_json(None, &["channels", "--format", "json"]);
    assert!(channels["channels"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == channel));
    let history = rig.atm_json(None, &["history", &channel, "--format", "json"]);
    let post = history["posts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap();
    assert_eq!(post["body"], "legacy channel read");
    assert_eq!(post["legacy"], true);
}

fn post_params(
    channel: Option<&str>,
    to: Option<&str>,
    reply_to: Option<&str>,
    body: &str,
) -> Value {
    json!({"channel":channel,"to":to,"reply_to":reply_to,"body":body,"kind":"note",
           "attachments":[],"task":null,"tags":[],"client_key":null})
}

fn post_result(channel: &str) -> Value {
    json!({"seq":502,"post_id":"m500","channel":channel,"thread":null,"mentions":[],
           "woke":[],"unavailable":[],"route":"none","unresolved":[]})
}

fn post_diagnostics() -> Value {
    json!({"seq":502,"post_id":"m500","channel":"alpha","thread":null,
        "mentions":[{"seat":"alpha/build-2"},{"seat":"beta/lead"}],
        "woke":[{"to":"alpha/build-2","message_id":"m501"},{"to":"beta/lead","message_id":"m502"}],
        "unavailable":[{"to":"alpha/build-2","state":"down","since":"2026-11-20T21:40:05Z","message_id":"m501"},
                       {"to":"beta/lead","state":"stuck","since":"2026-11-20T21:41:05Z","message_id":"m502"}],
        "route":"none","unresolved":[{"text":"@typo","reason":"unknown"}]})
}

fn agent(id: &str, pane: Option<&str>, state: &str) -> Value {
    json!({"agent_id":id,"state":state,"activity":null,"pane":pane,"session_id":null,
           "harness":null,"model":null,"cost_usd":null,"reason":null})
}

fn agent_list(rows: Vec<Value>) -> CliStep {
    ok("agent.list", json!({"project_root":null}), json!(rows))
}

// Break: allow multiple targets, or issue a request before input validation.
#[tokio::test]
async fn post_requires_body_and_excludes_target_pairs_before_connecting() {
    for targets in [
        ["--channel", "alpha", "--to", "owner"],
        ["--channel", "alpha", "--reply", "m1"],
        ["--to", "owner", "--reply", "m1"],
    ] {
        let mut args = vec!["post", "--body", "hello"];
        args.extend(targets);
        failure(run(&args, Some("%7"), vec![]).await, "cannot be used with");
    }
    for (args, diagnostic) in [
        (&["post", "--to", "owner"][..], "--body"),
        (
            &[
                "post", "--to", "owner", "--body", "hello", "--kind", "invalid",
            ][..],
            "invalid value",
        ),
        (
            &["post", "--channel", "alpha/lead+owner", "--body", "hello"][..],
            "invalid project id",
        ),
    ] {
        let out = run(args, None, vec![]).await;
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("unrecognized subcommand"),
            "{out:?}"
        );
        failure(out, diagnostic);
    }
}

// Break: substitute cwd/env identity or use a non-unique/down pane binding.
#[tokio::test]
async fn post_defaults_to_the_bound_seats_project_only() {
    let args = ["post", "--body", "hello"];
    let result = post_result("alpha");
    let bound = agent("alpha/worker", Some("%7"), "ready");
    let other = agent("beta/lead", Some("%8"), "ready");
    let out = run(
        &args,
        Some("%7"),
        vec![
            agent_list(vec![other.clone(), bound.clone()]),
            ok(
                "channel.post",
                post_params(Some("alpha"), None, None, "hello"),
                result,
            ),
        ],
    )
    .await;
    assert_eq!(
        text(out),
        "m500 posted channel=alpha thread=-; woke: none\n"
    );

    let explicit =
        "atm post without a target requires a named agent; use --channel, --to, or --reply";
    failure(run(&args, None, vec![]).await, explicit);
    failure(
        run(&args, Some("%7"), vec![agent_list(vec![other])]).await,
        explicit,
    );
    failure(
        run(
            &args,
            Some("%7"),
            vec![agent_list(vec![agent("alpha/worker", Some("%7"), "down")])],
        )
        .await,
        explicit,
    );
    failure(
        run(
            &args,
            Some("%7"),
            vec![agent_list(vec![
                bound,
                agent("beta/lead", Some("%7"), "ready"),
            ])],
        )
        .await,
        "caller pane has ambiguous agent bindings",
    );
}

// Break: resolve in the wrong project, do a lookup for full IDs, or send to system.
#[tokio::test]
async fn post_to_reuses_msg_send_address_resolution() {
    let args = ["post", "--to", "lead", "--body", "hello"];
    let expected = |to| {
        ok(
            "channel.post",
            post_params(None, Some(to), None, "hello"),
            post_result("alpha/lead+owner"),
        )
    };
    let alpha = agent("alpha/lead", Some("%8"), "ready");
    let beta = agent("beta/lead", Some("%9"), "ready");
    assert!(!text(
        run(
            &args,
            Some("%7"),
            vec![
                agent_list(vec![
                    beta.clone(),
                    alpha,
                    agent("alpha/worker", Some("%7"), "ready")
                ]),
                expected("alpha/lead")
            ]
        )
        .await
    )
    .is_empty());

    let dir = tempfile::tempdir().unwrap();
    text(
        run_in(
            dir.path(),
            &args,
            None,
            vec![
                ok(
                    "agent.list",
                    json!({"project_root":dir.path()}),
                    json!([beta]),
                ),
                expected("beta/lead"),
            ],
        )
        .await,
    );
    for to in ["beta/lead", "owner"] {
        text(
            run(
                &["post", "--to", to, "--body", "hello"],
                None,
                vec![expected(to)],
            )
            .await,
        );
    }
    failure(
        run(&["post", "--to", "system", "--body", "hello"], None, vec![]).await,
        "system is not a post recipient",
    );
}

// Break: drop/reorder wake diagnostics or mistake pending for a final empty set.
#[tokio::test]
async fn post_text_reports_each_wake_unavailable_and_unresolved() {
    let args = [
        "post",
        "--channel",
        "alpha",
        "--body",
        "ready @typo",
        "--kind",
        "report",
        "--attach",
        "brief.md",
        "--task",
        "t7",
        "--tag",
        "build-2",
        "--tag",
        "beta/lead",
    ];
    let params = json!({"channel":"alpha","to":null,"reply_to":null,"body":"ready @typo","kind":"report",
        "attachments":["brief.md"],"task":"t7","tags":["build-2","beta/lead"],"client_key":null});
    let out = run(
        &args,
        Some("%7"),
        vec![ok("channel.post", params, post_diagnostics())],
    )
    .await;
    assert_eq!(
        text(out),
        concat!(
            "m500 posted channel=alpha thread=-; woke: alpha/build-2, beta/lead\n",
            "build-2 is down since 21:40 UTC: wake m501 is queued and its manager is told\n",
            "beta/lead is stuck since 21:41 UTC: wake m502 is queued and its manager is told\n",
            "unresolved: @typo (unknown)\n"
        )
    );
    for (route, expected) in [("pending", "none yet"), ("none", "none")] {
        let mut result = post_result("alpha");
        result["route"] = json!(route);
        let out = run(
            &["post", "--channel", "alpha", "--body", "hello"],
            None,
            vec![ok(
                "channel.post",
                post_params(Some("alpha"), None, None, "hello"),
                result,
            )],
        )
        .await;
        assert_eq!(
            text(out),
            format!("m500 posted channel=alpha thread=-; woke: {expected}\n")
        );
    }
}

// Break: rewrite a wake ID, trim or read literal inputs, or drop future result keys.
#[tokio::test]
async fn post_json_keeps_the_raw_result_and_replies_with_unmodified_input() {
    let body = "  first\n\n雪 \"quoted\"\n";
    let args = [
        "post",
        "--reply",
        "m501",
        "--body",
        body,
        "--kind",
        "review",
        "--task",
        "t7",
        "--tag",
        "build-2",
        "--tag",
        "beta/lead",
        "--attach",
        "brief.md",
        "--attach",
        "not a file",
        "--format",
        "json",
    ];
    let params = json!({"channel":null,"to":null,"reply_to":"m501","body":body,"kind":"review",
        "attachments":["brief.md","not a file"],"task":"t7","tags":["build-2","beta/lead"],"client_key":null});
    let mut result = post_diagnostics();
    result["thread"] = json!("m480");
    result["unavailable"][0]["future"] = json!({"value":null});
    raw_json(
        run(
            &args,
            Some("%7"),
            vec![ok("channel.post", params, result.clone())],
        )
        .await,
        &result,
    );

    let mut params = post_params(None, Some("owner"), None, "");
    params["attachments"] = json!(["brief.md"]);
    text(
        run(
            &[
                "post", "--to", "owner", "--body", "", "--attach", "brief.md",
            ],
            Some("%7"),
            vec![ok(
                "channel.post",
                params,
                post_result("alpha/worker+owner"),
            )],
        )
        .await,
    );
}

// Break: making the share bit a top-level option, dropping it from a reply,
// or changing an old daemon's rejected flagged write to an ordinary reply.
#[tokio::test]
async fn post_also_in_chat_requires_reply_and_is_not_retried() {
    for target in [vec![], vec!["--channel", "alpha"], vec!["--to", "owner"]] {
        let reason = if target.is_empty() {
            "--reply"
        } else {
            "cannot be used with"
        };
        let mut args = vec!["post", "--body", "hi", "--also-in-chat"];
        args.extend(target);
        failure(run(&args, None, vec![]).await, reason);
    }
    let args = ["post", "--reply", "m13", "--also-in-chat", "--body", "hi"];
    let mut params = post_params(None, None, Some("m13"), "hi");
    params["also_in_chat"] = json!(true);
    let mut receipt = post_result("alpha");
    receipt["thread"] = json!("m13");
    assert!(text(
        run(
            &args,
            Some("%7"),
            vec![ok("channel.post", params.clone(), receipt)]
        )
        .await
    )
    .contains("thread=m13"));
    failure(run(&args, Some("%7"), vec![CliStep::Response {
        method: "channel.post", params,
        payload: json!({"error":{"code":"invalid_params","message":"unknown field `also_in_chat`"}}),
    }]).await, "unknown field `also_in_chat`");
}

// Break: retry a refusal/timeout, suggest the wrong recovery, or send to an old daemon.
#[tokio::test]
async fn post_refusal_and_old_daemon_are_not_retried() {
    let args = [
        "post",
        "--channel",
        "alpha",
        "--tag",
        "bad",
        "--body",
        "hello",
    ];
    let mut params = post_params(Some("alpha"), None, None, "hello");
    params["tags"] = json!(["bad"]);
    let error = "tag bad does not resolve: unknown";
    failure(
        run(
            &args,
            Some("%7"),
            vec![CliStep::Response {
                method: "channel.post",
                params,
                payload: json!({"error":{"code":"invalid_params","message":error}}),
            }],
        )
        .await,
        error,
    );
    failure(
        run(&args, Some("%7"), vec![CliStep::TooOld]).await,
        "the daemon is too old; restart atmd",
    );
    let timeout = run(
        &["post", "--reply", "m501", "--body", "hello"],
        Some("%7"),
        vec![CliStep::Response {
            method: "channel.post",
            params: post_params(None, None, Some("m501"), "hello"),
            payload: json!({"error":{"code":"timeout","message":"message request timed out; accepted work may have completed; inspect inbox before retrying"}}),
        }],
    )
    .await;
    failure(
        timeout,
        "check `atm history <channel>` (add `--thread <id>` for a reply) before retrying",
    );
}

fn inbox_params() -> Value {
    json!({"for":null,"unread_only":false,"count_only":false,"id":null,"after":null,"limit":100})
}

fn inbox_row() -> Value {
    json!({"message_id":"m501",
        "content":{"from":"alpha/lead","to":"alpha/build-2","kind":"note","body":"ready",
            "attachments":[],"task":null},
        "sent_at":"2026-11-20T21:42:00Z","status":"delivered","read_at":null,
        "delivery":{"phase":"delivered","reason":null,"attempt":1,"pane":"%7","session_id":null,
            "confirmation":null,"retry_at":null,"blocked_by":null}})
}

fn wake_row() -> Value {
    let mut row = inbox_row();
    row["content"]["post"] = json!("m500");
    row["post_context"] = json!({"channel":"alpha","thread":"m480","session_id":"s-author"});
    row
}

fn inbox_page(rows: Vec<Value>) -> Value {
    json!({"unread_count":rows.len(),"messages":rows,"next_after":null})
}

// Break: use the wake/root ID in the hint, or ask for membership for every row.
#[tokio::test]
async fn inbox_wake_text_uses_post_id_and_checks_permission_once() {
    let mut root = wake_row();
    root["message_id"] = json!("m504");
    root["content"]["post"] = json!("m503");
    root["post_context"]["thread"] = Value::Null;
    let mut plain = inbox_row();
    plain["message_id"] = json!("m506");
    let project = || {
        ok(
            "channel.list",
            list_params(false),
            list_page(vec![project_row()]),
        )
    };
    let page = inbox_page(vec![wake_row(), root, plain]);
    let multi = text(
        run(
            &["inbox"],
            Some("%7"),
            vec![ok("msg.inbox", inbox_params(), page), project()],
        )
        .await,
    );
    assert_eq!(
        multi
            .lines()
            .filter(|l| l.starts_with("post:") || l.starts_with("reply:"))
            .collect::<Vec<_>>(),
        [
            "post: m500 channel=alpha thread=m480",
            "reply: atm post --reply m500 --body '...'",
            "post: m503 channel=alpha thread=-",
            "reply: atm post --reply m503 --body '...'",
        ]
    );
    let mut args = inbox_params();
    args["id"] = json!("m501");
    let one = text(
        run(
            &["inbox", "--id", "m501"],
            Some("%7"),
            vec![
                ok("msg.inbox", args, inbox_page(vec![wake_row()])),
                project(),
            ],
        )
        .await,
    );
    assert_eq!(one, concat!(
        "m501 from=alpha/lead to=alpha/build-2 kind=note status=delivered sent=2026-11-20T21:42:00+00:00 read=-\n",
        "delivery: delivered pane=%7 attempt=1\n",
        "post: m500 channel=alpha thread=m480\n",
        "reply: atm post --reply m500 --body '...'\n  ready\n1 unread\n"
    ));
}

// Break: a writable DM must not force a thread, including the Leads section.
#[tokio::test]
async fn inbox_writable_direct_replies_to_author_not_thread() {
    let id = "alpha/lead+owner";
    let mut channel = direct_row(id, "alpha/lead", "owner");
    channel["section"] = json!("lead");
    channel["member"] = json!(true);
    channel["writable"] = json!(true);
    let mut reply = wake_row();
    reply["content"]["to"] = json!("owner");
    reply["post_context"]["channel"] = json!(id);
    let mut root = reply.clone();
    root["message_id"] = json!("m504");
    root["content"]["post"] = json!("m503");
    root["post_context"]["thread"] = Value::Null;
    let out = text(
        run(
            &["inbox"],
            None,
            vec![
                ok("msg.inbox", inbox_params(), inbox_page(vec![reply, root])),
                ok("channel.list", list_params(false), list_page(vec![channel])),
            ],
        )
        .await,
    );
    assert_eq!(
        out.lines()
            .filter(|l| l.starts_with("post:") || l.starts_with("reply:"))
            .collect::<Vec<_>>(),
        [
            "post: m500 channel=alpha/lead+owner thread=m480",
            "reply: atm post --to alpha/lead --body '...'",
            "post: m503 channel=alpha/lead+owner thread=-",
            "reply: atm post --to alpha/lead --body '...'",
        ]
    );
}

// Break: infer permission from `to`, the --for target, or the existence of a channel row.
#[tokio::test]
async fn inbox_outside_channel_replies_to_author_as_actual_caller() {
    let mut outside = wake_row();
    outside["content"]["to"] = json!("beta/build-2");
    let foreign = run(
        &["inbox"],
        Some("%7"),
        vec![
            ok("msg.inbox", inbox_params(), inbox_page(vec![outside])),
            ok("channel.list", list_params(false), list_page(vec![])),
        ],
    )
    .await;
    let hint = "reply: atm post --to alpha/lead --body '...'";
    assert_eq!(
        text(foreign).lines().find(|l| l.starts_with("reply:")),
        Some(hint)
    );

    let channel = "alpha/build-2+alpha/lead";
    let mut row = wake_row();
    row["post_context"]["channel"] = json!(channel);
    let mut args = inbox_params();
    args["for"] = json!("alpha/build-2");
    let inspected = run(
        &["inbox", "--for", "alpha/build-2"],
        None,
        vec![
            ok("msg.inbox", args, inbox_page(vec![row])),
            ok(
                "channel.list",
                list_params(false),
                list_page(vec![direct_row(channel, "alpha/build-2", "alpha/lead")]),
            ),
        ],
    )
    .await;
    assert_eq!(
        text(inspected).lines().find(|l| l.starts_with("reply:")),
        Some(hint)
    );
}

// Break: optional lookup errors hide the inbox, or an unknown channel/system author gets a hint.
#[tokio::test]
async fn inbox_no_reply_on_lookup_failure_unknown_channel_or_system_fallback() {
    for refusal in [
        CliStep::TooOld,
        CliStep::Response {
            method: "channel.list",
            params: list_params(false),
            payload: json!({"error":{"code":"refused","message":"channel list is over budget"}}),
        },
    ] {
        let out = text(
            run(
                &["inbox"],
                Some("%7"),
                vec![
                    ok("msg.inbox", inbox_params(), inbox_page(vec![wake_row()])),
                    refusal,
                ],
            )
            .await,
        );
        assert!(
            out.contains("post: m500 channel=alpha thread=m480\n  ready\n"),
            "{out:?}"
        );
        assert!(!out.contains("reply:"), "{out:?}");
    }
    let mut later = wake_row();
    later["post_context"]["channel"] = json!("other\nline\u{1b}");
    let out = text(
        run(
            &["inbox"],
            Some("%7"),
            vec![ok("msg.inbox", inbox_params(), inbox_page(vec![later]))],
        )
        .await,
    );
    assert!(
        out.contains("post: m500 channel=other\\nline\\u{1b} thread=m480\n"),
        "{out:?}"
    );
    assert!(!out.contains("reply:"), "{out:?}");

    let mut system = wake_row();
    system["message_id"] = json!("m505");
    system["content"]["post"] = json!("m504");
    system["content"]["from"] = json!("system");
    let out = text(
        run(
            &["inbox"],
            Some("%7"),
            vec![
                ok(
                    "msg.inbox",
                    inbox_params(),
                    inbox_page(vec![wake_row(), system]),
                ),
                ok(
                    "channel.list",
                    list_params(false),
                    list_page(vec![project_row()]),
                ),
            ],
        )
        .await,
    );
    assert!(
        out.contains("post: m504 channel=alpha thread=m480\n  ready\n"),
        "{out:?}"
    );
    assert_eq!(
        out.lines()
            .filter(|line| line.starts_with("reply:"))
            .count(),
        1
    );
}

// Guard: JSON stays a single read and contains the original wake and post-context fields.
#[tokio::test]
async fn inbox_wake_json_is_unmodified_and_does_not_lookup() {
    let mut row = wake_row();
    row["post_context"]["future"] = json!({"value":null});
    let expected = inbox_page(vec![row]);
    raw_json(
        run(
            &["inbox", "--format", "json"],
            Some("%7"),
            vec![ok("msg.inbox", inbox_params(), expected.clone())],
        )
        .await,
        &expected,
    );
}

// Guard: old and redacted rows still match the old rendering byte for byte.
#[tokio::test]
async fn plain_partial_and_redacted_inbox_bytes_are_unchanged() {
    let mut plain = inbox_row();
    plain["content"]["task"] = json!("t7");
    plain["content"]["attachments"] = json!(["brief.md"]);
    let mut linked = plain.clone();
    linked["content"]["post"] = json!("m500");
    let mut joined = plain.clone();
    joined["post_context"] = wake_row()["post_context"].clone();
    let mut redacted = inbox_row();
    redacted["content"]["body"] = json!("");
    for (row, show, body) in [
        (plain, None, "task: t7\nattach: brief.md\n  ready\n"),
        (linked, None, "task: t7\nattach: brief.md\n  ready\n"),
        (joined, None, "task: t7\nattach: brief.md\n  ready\n"),
        (redacted, Some("alpha/build-2"), "  \n"),
    ] {
        let mut params = inbox_params();
        params["for"] = json!(show);
        params["unread_only"] = json!(true);
        let mut expected = inbox_page(vec![row]);
        expected["next_after"] = json!("m501");
        let mut args = vec!["inbox", "--unread"];
        let mut next = "next:".to_owned();
        if let Some(id) = show {
            args.extend(["--for", id]);
            next.push_str(" --for alpha/build-2");
        }
        let out = run(
            &args,
            Some("%7"),
            vec![ok("msg.inbox", params.clone(), expected.clone())],
        )
        .await;
        assert_eq!(text(out), format!(concat!(
            "m501 from=alpha/lead to=alpha/build-2 kind=note status=delivered sent=2026-11-20T21:42:00+00:00 read=-\n",
            "delivery: delivered pane=%7 attempt=1\n{}1 unread\n{} --unread --after m501\n"), body, next));
        args.extend(["--format", "json"]);
        let out = run(
            &args,
            Some("%7"),
            vec![ok("msg.inbox", params, expected.clone())],
        )
        .await;
        assert_eq!(text(out), format!("{expected}\n"));
    }
}

// Guard: the legacy send RPC and both output modes do not become post-shaped.
#[tokio::test]
async fn msg_send_bytes_are_unchanged() {
    for (to, format, expected) in [
        ("alpha/lead", "text", "m90 queued\n"),
        ("owner", "text", "m90 delivered to owner\n"),
        ("alpha/lead", "json", "{\"message_id\":\"m90\"}\n"),
    ] {
        let step = ok(
            "msg.send",
            json!({"to":to,"kind":"note","body":"hello","attachments":[],"task":null}),
            json!({"message_id":"m90"}),
        );
        let out = run(
            &[
                "msg", "send", "--to", to, "--body", "hello", "--format", format,
            ],
            Some("%7"),
            vec![step],
        )
        .await;
        assert_eq!(text(out), expected);
    }
}

// Break: route the alias through a lookup, a different RPC, or rewrite the IDs.
#[tokio::test]
async fn ack_and_msg_ack_forward_the_same_ids_without_lookup() {
    for (prefix, method) in [(vec!["msg", "ack"], "msg.ack"), (vec!["ack"], "wake.ack")] {
        let ids = json!({"ids":["m500","m501","m90"]});
        let mut args = prefix.clone();
        args.extend(["m500", "m501", "m90"]);
        for (format, count, want) in [
            ("text", 1, "1 marked read\n"),
            ("text", 0, "0 marked read\n"),
            ("json", 1, "{\"count\":1}\n"),
        ] {
            let mut cmd = args.clone();
            cmd.extend(["--format", format]);
            let result = ok(method, ids.clone(), json!({"count":count}));
            assert_eq!(text(run(&cmd, Some("%7"), vec![result]).await), want);
        }
        let message = "post m500 has no inbox row for alpha/worker";
        failure(
            run(
                &args,
                Some("%7"),
                vec![CliStep::Response {
                    method,
                    params: ids,
                    payload: json!({"error":{"code":"refused","message":message}}),
                }],
            )
            .await,
            message,
        );
        failure(run(&prefix, Some("%7"), vec![]).await, "<IDS>...");
        let mut invalid = prefix;
        invalid.push("m01");
        failure(run(&invalid, Some("%7"), vec![]).await, "m01");
    }
}

const LEGACY_WAKE_METHODS: &[&str] = &["msg.inbox", "msg.ack", "msg.retry", "msg.give_up"];
const ALL_WAKE_METHODS: &[&str] = &[
    "msg.inbox",
    "msg.ack",
    "msg.retry",
    "msg.give_up",
    "wake.list",
    "wake.ack",
    "wake.retry",
    "wake.give_up",
];

async fn run_wake(args: &[&str], methods: &[&str], steps: Vec<CliStep>) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let pane = Some("%7");
    let command = task_cli::command(dir.path(), args, pane);
    run_cli_script_with_methods(dir.path(), command, pane, steps, Some(methods)).await
}

struct WakeCliCase {
    new_args: &'static [&'static str],
    old_args: &'static [&'static str],
    new_method: &'static str,
    old_method: &'static str,
    params: Value,
    reply: Value,
    text: &'static str,
}

fn wake_cli_cases() -> [WakeCliCase; 4] {
    [
        WakeCliCase {
            new_args: &["ack", "m500", "m7"],
            old_args: &["msg", "ack", "m500", "m7"],
            new_method: "wake.ack",
            old_method: "msg.ack",
            params: json!({"ids":["m500","m7"]}),
            reply: json!({"count":1,"future":null}),
            text: "1 marked read\n",
        },
        WakeCliCase {
            new_args: &["wake", "retry", "m7", "--accept-duplicate-risk"],
            old_args: &["msg", "retry", "m7", "--accept-duplicate-risk"],
            new_method: "wake.retry",
            old_method: "msg.retry",
            params: json!({"id":"m7"}),
            reply: json!({"message_id":"m7","status":"queued","future":null}),
            text: "m7 queued (duplicate delivery possible)\n",
        },
        WakeCliCase {
            new_args: &["wake", "give-up", "m7"],
            old_args: &["msg", "give-up", "m7"],
            new_method: "wake.give_up",
            old_method: "msg.give_up",
            params: json!({"id":"m7"}),
            reply: json!({"message_id":"m7","status":"failed","editor_warning":"inspect the editor","future":null}),
            text: "m7 failed\nwarning: inspect the editor\n",
        },
        // s3-giverelay: the designated relay gives up in the owner's words.
        WakeCliCase {
            new_args: &[
                "wake",
                "give-up",
                "--relay",
                "--confirm",
                "m7",
                "Cancel them",
            ],
            old_args: &[
                "msg",
                "give-up",
                "--relay",
                "--confirm",
                "m7",
                "Cancel them",
            ],
            new_method: "wake.give_up",
            old_method: "msg.give_up",
            params: json!({"id":"m7","relay_words":"Cancel them"}),
            reply: json!({"message_id":"m7","status":"failed","editor_warning":"inspect the editor","future":null}),
            text: "m7 failed\nwarning: inspect the editor\n",
        },
    ]
}

async fn assert_wake_case(
    case: &WakeCliCase,
    args: &[&str],
    method: &'static str,
    methods: &[&str],
    missing_final: bool,
) {
    for format in ["text", "json"] {
        let mut cmd = args.to_vec();
        cmd.extend(["--format", format]);
        let mut steps = Vec::new();
        if missing_final {
            steps.push(CliStep::TooOld);
        }
        steps.push(ok(method, case.params.clone(), case.reply.clone()));
        let out = run_wake(&cmd, methods, steps).await;
        if format == "json" {
            raw_json(out, &case.reply);
        } else {
            assert_eq!(text(out), case.text);
        }
    }
}

// Break: choosing the legacy method when both exist, or changing the legacy commands.
#[tokio::test]
async fn wake_commands_prefer_final_names() {
    for case in wake_cli_cases() {
        for (args, method) in [
            (case.new_args, case.new_method),
            (case.old_args, case.old_method),
        ] {
            assert_wake_case(&case, args, method, ALL_WAKE_METHODS, false).await;
        }
    }
}

// Break: sending a new request to the old daemon, failing to fall back, or repeating a mutation.
#[tokio::test]
async fn wake_commands_against_rollback_daemon() {
    for case in wake_cli_cases() {
        assert_wake_case(
            &case,
            case.new_args,
            case.old_method,
            LEGACY_WAKE_METHODS,
            true,
        )
        .await;
    }
    failure(
        run_wake(&["ack", "m7"], &[], vec![CliStep::TooOld, CliStep::TooOld]).await,
        "the daemon is too old; restart atmd",
    );
}

// Break: using a post-send error as a capability check, or connecting before local validation.
#[tokio::test]
async fn wake_errors_do_not_fall_back() {
    for code in ["refused", "unknown_method", "timeout"] {
        let message = format!("{code}: the daemon is too old; restart atmd");
        let step = CliStep::Response {
            method: "wake.retry",
            params: json!({"id":"m7"}),
            payload: json!({"error":{"code":code,"message":message}}),
        };
        failure(
            run_wake(
                &["wake", "retry", "m7", "--accept-duplicate-risk"],
                ALL_WAKE_METHODS,
                vec![step],
            )
            .await,
            &message,
        );
    }
    failure(
        run_wake(
            &["ack", "m7"],
            ALL_WAKE_METHODS,
            vec![ok(
                "wake.ack",
                json!({"ids":["m7"]}),
                json!({"count":false}),
            )],
        )
        .await,
        "invalid message result from daemon",
    );
    for (args, diagnostic) in [
        (
            &["wake", "retry", "m7"][..],
            "Repeat with --accept-duplicate-risk.",
        ),
        (
            &["wake", "retry", "m01", "--accept-duplicate-risk"][..],
            "m01",
        ),
        (&["wake", "give-up"][..], "<ID>"),
        // s3-giverelay: a partial relay form never connects.
        (
            &["wake", "give-up", "--relay", "m7", "Cancel them"][..],
            "--confirm",
        ),
        (
            &["wake", "give-up", "--relay", "--confirm", "m7"][..],
            "<WORDS>",
        ),
        (&["wake", "give-up", "m7", "Cancel them"][..], "--relay"),
    ] {
        failure(run_wake(args, ALL_WAKE_METHODS, vec![]).await, diagnostic);
    }
}

// Break: the real daemon cannot use the CLI's default project, find the linked
// wake, offer a reply, or acknowledge the caller's row by post ID.
#[test]
fn real_post_inbox_and_ack_by_post_id() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", stand_in::StandInMode::Pi);
    let worker = rig.ready_agent("build-2", stand_in::StandInMode::Pi);
    let channel = lead.id.split_once('/').unwrap().0;
    let body = "Channel update: only the inbox gets this body.";
    let mut watch = rig.watch();
    let cursor = rig.test.rig.event_cursor(&worker.pane);

    let post = rig.atm_json(
        Some(&lead.pane),
        &[
            "post", "--tag", "build-2", "--body", body, "--format", "json",
        ],
    );
    assert_eq!(post["channel"], channel);
    assert_eq!(post["route"], "none");
    let post_id = post["post_id"].as_str().unwrap();
    let [woke] = post["woke"].as_array().unwrap().as_slice() else {
        panic!("{post}");
    };
    assert_eq!(woke["to"], worker.id);
    let wake_id = woke["message_id"].as_str().unwrap();
    assert_ne!(post_id, wake_id);
    let written = watch.wait("the channel post", |e| {
        e["kind"] == "channel.posted" && e["body"]["body"] == body
    });
    assert_eq!(format!("m{}", written["seq"]), post_id);
    assert_eq!(written["body"]["author"], lead.id);

    let delivered = watch.wait("the wake receipt", about("msg.delivered", wake_id));
    assert_eq!(
        delivered["body"]["confirmation"],
        "prompt_submit_exact_reference"
    );
    let line = reference(wake_id);
    rig.test.rig.wait_event(&worker.pane, cursor, |e| {
        e["kind"] == "accepted" && e["text"] == line
    });
    assert_eq!(rig.accepted_since(&worker.pane, cursor), [line]);
    let inbox = text(rig.atm(Some(&worker.pane), &["inbox", "--id", wake_id]));
    assert!(inbox.contains(&format!("post: {post_id} channel={channel} thread=-\nreply: atm post --reply {post_id} --body '...'\n  {body}\n")), "{inbox}");
    assert!(rig.inbox_entry(Some(&worker.pane), None, wake_id)["read_at"].is_null());
    let lookup = json!({"id":wake_id});
    let legacy = rig
        .call("msg.inbox", lookup.clone(), Some(&worker.pane))
        .unwrap();
    let wake = rig.call("wake.list", lookup, Some(&worker.pane)).unwrap();
    assert_eq!(wake, legacy, "wake.list must preserve the joined inbox row");

    let ack = ["ack", post_id, "--format", "json"];
    assert_eq!(rig.atm_json(Some(&worker.pane), &ack)["count"], 1);
    let read = watch.wait("read of the wake row", |e| {
        e["kind"] == "msg.read" && e["body"]["message_ids"] == json!([wake_id])
    });
    assert_eq!(read["body"]["message_ids"], json!([wake_id]));
    assert!(rig.inbox_entry(Some(&worker.pane), None, wake_id)["read_at"].is_string());
    assert_eq!(rig.atm_json(Some(&worker.pane), &ack)["count"], 0);
    rig.finish_turn(&worker.pane);
    rig.test.rig.close();
}
