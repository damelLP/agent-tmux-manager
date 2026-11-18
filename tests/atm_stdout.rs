//! Stdout-only failure policy over real pipes and private scripted daemon peers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};
use std::{
    io,
    os::fd::AsRawFd,
    path::Path,
    process::{Output, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
    sync::oneshot,
};

// No concurrently forked fixture child may inherit a briefly open pipe reader.
static SPAWN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone)]
struct Step {
    method: &'static str,
    params: Option<Value>,
    reply: Value,
}
fn rpc(method: &'static str, result: Value) -> Step {
    Step {
        method,
        params: None,
        reply: json!({"type":"response","id":"r1","result":result}),
    }
}
fn checked_rpc(method: &'static str, params: Value, result: Value) -> Step {
    Step {
        params: Some(params),
        ..rpc(method, result)
    }
}
fn sessions() -> Step {
    Step {
        method: "list_sessions",
        params: None,
        reply: json!({"type":"session_list","sessions":[atm_core::SessionView::default()]}),
    }
}
#[derive(Clone, Copy)]
enum Sink {
    Capture,
    Closed,
    Full,
}

fn command(dir: &Path, args: &[&str], backtrace: bool) -> Command {
    use std::os::unix::fs::PermissionsExt;
    for sub in ["home", "state", "config", "tmp", "tmux", "bin"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    for name in ["atmd", "git", "tmux"] {
        let file = dir.join("bin").join(name);
        // Legacy list JSON asks tmux for coordinator metadata. Never a real server.
        std::fs::write(&file, "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atm"));
    cmd.args(args)
        .env_clear()
        .current_dir(dir)
        .env("HOME", dir.join("home"))
        .env("SHELL", "/bin/sh")
        .env("ATM_SOCKET", dir.join("rpc.sock"))
        .env("ATM_STATE_DIR", dir.join("state"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("TMPDIR", dir.join("tmp"))
        .env("TMUX_TMPDIR", dir.join("tmux"))
        .env("ATM_TMUX_SOCKET", "stdout-test")
        .env("TMUX_PANE", "%7")
        .env(
            "TMUX",
            format!("{},0,0", dir.join("tmux/private").display()),
        )
        .env("ATM_SPAWN_PI_BIN", "/bin/true")
        .env("PATH", dir.join("bin"))
        .env("RUST_BACKTRACE", if backtrace { "1" } else { "0" })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

async fn exchange(
    args: &[&str],
    steps: Vec<Step>,
    sink: Sink,
    backtrace: bool,
    break_request: bool,
) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("rpc.sock")).unwrap();
    let (exited_tx, mut exited_rx) = oneshot::channel();
    let root = dir.path().to_owned();
    let peer = tokio::spawn(async move {
        for step in steps {
            let (mut read, mut write) = loop {
                let (stream, _) = tokio::select! {
                    biased;
                    accepted = listener.accept() => accepted.unwrap(),
                    _ = &mut exited_rx => panic!("child exited before {}", step.method),
                };
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
            if break_request {
                // Complete read shutdown BEFORE advertising capabilities: the next
                // client request deterministically hits EPIPE, not a racing EOF.
                assert_eq!(
                    unsafe { libc::shutdown(read.get_ref().as_ref().as_raw_fd(), libc::SHUT_RD) },
                    0
                );
            }
            write.write_all(format!("{}\n", json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"stdout-test","methods":[step.method]})).as_bytes()).await.unwrap();
            if break_request {
                exited_rx.await.unwrap();
                return;
            }
            let mut line = String::new();
            assert_ne!(
                read.read_line(&mut line).await.unwrap(),
                0,
                "missing {}",
                step.method
            );
            let request: Value = serde_json::from_str(&line).unwrap();
            if step.method == "list_sessions" {
                assert_eq!(request["type"], "list_sessions");
            } else {
                assert_eq!(request["type"], "request");
                assert_eq!(request["method"], step.method);
                assert_eq!(request["id"], "r1");
                assert_eq!(request["from_pane"], "%7");
                if let Some(params) = step.params {
                    assert_eq!(request["params"], params);
                }
                if step.method.starts_with("session.") {
                    let socket = root.join(format!("tmux/tmux-{}/stdout-test", unsafe {
                        libc::getuid()
                    }));
                    assert_eq!(request["params"]["caller_server"], json!(socket));
                }
                if step.method == "session.spawn_type" {
                    assert_eq!(request["params"]["pane"], "%42");
                    assert!(request["params"]["command"]
                        .as_str()
                        .unwrap()
                        .contains("/bin/true"));
                }
            }
            write
                .write_all(format!("{}\n", step.reply).as_bytes())
                .await
                .unwrap();
            line.clear();
            // Legacy session readers send an explicit disconnect before EOF.
            if step.method == "list_sessions" {
                assert_ne!(read.read_line(&mut line).await.unwrap(), 0);
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap()["type"],
                    "disconnect"
                );
                line.clear();
            }
            assert_eq!(
                read.read_line(&mut line).await.unwrap(),
                0,
                "extra request: {line}"
            );
        }
        exited_rx.await.unwrap();
        std::future::poll_fn(|cx| {
            assert!(
                listener.poll_accept(cx).is_pending(),
                "unexpected extra connection"
            );
            std::task::Poll::Ready(())
        })
        .await;
    });
    let mut cmd = command(dir.path(), args, backtrace);
    let child = {
        let _spawn = SPAWN_LOCK.lock().unwrap();
        match sink {
            Sink::Capture => {}
            Sink::Closed => {
                let (reader, writer) = io::pipe().unwrap();
                drop(reader); // A real pipe with no reader, before spawning the child.
                cmd.stdout(writer);
            }
            Sink::Full => {
                cmd.stdout(
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open("/dev/full")
                        .unwrap(),
                );
            }
        }
        cmd.spawn().unwrap()
    };
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let _ = exited_tx.send(());
    tokio::time::timeout(Duration::from_secs(60), peer)
        .await
        .unwrap()
        .unwrap();
    output
}

fn context() -> Value {
    serde_json::from_str(include_str!("fixtures/atm_me/context.json")).unwrap()
}
fn tree() -> Value {
    serde_json::from_str(include_str!("fixtures/atm_org/tree.json")).unwrap()
}
fn decisions() -> Value {
    serde_json::from_str(include_str!("fixtures/atm_decide/list.json")).unwrap()
}
fn inbox() -> Value {
    json!({"messages":[],"unread_count":0,"next_after":null})
}
fn fallible_cases() -> Vec<(Vec<&'static str>, Step)> {
    let mut resolved = decisions()["decisions"][0].clone();
    resolved["status"] = json!("resolved");
    resolved["closed_at"] = json!("2026-01-15T12:01:00Z");
    resolved["resolution"] =
        json!({"decision_id":"d41","option":"yes","by":"owner","note":null,"standing_rule":false});
    vec![
        (vec!["org"], rpc("org.tree", tree())),
        (
            vec!["report"],
            rpc(
                "org.report",
                serde_json::from_str(include_str!("fixtures/atm_report/report.json")).unwrap(),
            ),
        ),
        (vec!["me"], rpc("agent.context", context())),
        (vec!["inbox"], rpc("msg.inbox", inbox())),
        (vec!["msg", "ack", "m7"], rpc("msg.ack", json!({"count":1}))),
        (
            vec!["task", "list"],
            rpc("task.list", json!({"tasks":[],"next_after":null})),
        ),
        (vec!["decisions"], rpc("decision.list", decisions())),
        (
            vec!["decide", "d41", "yes"],
            rpc(
                "decision.resolve",
                json!({"decision":resolved,"changed":true}),
            ),
        ),
        (
            vec!["away"],
            rpc(
                "owner.away",
                json!({"changed":true,"away_since":"2026-01-15T12:00:00Z","stacking_caps":{},"pending_count":0}),
            ),
        ),
        (
            vec!["back"],
            rpc(
                "owner.back",
                json!({"changed":false,"away_since":null,"provisional_count":0,"pending_count":0}),
            ),
        ),
        (
            vec!["agent", "clear", "project/lead"],
            rpc(
                "agent.clear",
                json!({"agent_id":"project/lead","session_id":null}),
            ),
        ),
    ]
}
fn legacy_cases() -> Vec<(Vec<&'static str>, Vec<Step>)> {
    vec![
        (
            vec!["kill", "%7"],
            vec![rpc("session.kill", json!({"pane":"%7"}))],
        ),
        (
            vec!["interrupt", "%7"],
            vec![rpc("session.interrupt", json!({"pane":"%7"}))],
        ),
        (
            vec!["send", "%7", "hello"],
            vec![rpc("session.send", json!({"pane":"%7"}))],
        ),
        (
            vec!["reply", "%7"],
            vec![rpc("session.reply", json!({"action":"enter","pane":"%7"}))],
        ),
        (
            vec!["reply", "%7", "--no"],
            vec![rpc("session.reply", json!({"action":"escape","pane":"%7"}))],
        ),
        (
            vec!["reply", "%7", "--option", "2"],
            vec![rpc(
                "session.reply",
                json!({"action":"selected","pane":"%7","option":2}),
            )],
        ),
        (
            vec!["peek", "%7"],
            vec![rpc("session.capture", json!({"lines":["Visible output."]}))],
        ),
        (vec!["list"], vec![sessions()]),
        (vec!["list", "--format", "ids"], vec![sessions()]),
        (vec!["list", "--format", "json"], vec![sessions()]),
        (
            vec!["status"],
            vec![
                sessions(),
                rpc("msg.inbox", inbox()),
                rpc(
                    "decision.list",
                    json!({"decisions":[],"pending_count":0,"provisional_count":0,"next_after":null}),
                ),
            ],
        ),
    ]
}
fn quiet(output: &Output, args: &[&str]) {
    assert_eq!(output.status.code(), Some(141), "{args:?}: {output:?}");
    assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
}

// Break caught: a fallible command bypasses the stdout helper or main traces EPIPE.
#[tokio::test]
async fn closed_stdout_is_quiet_in_every_fallible_command_family() {
    for (args, step) in fallible_cases() {
        for format in ["text", "json"] {
            for backtrace in [false, true] {
                let mut args = args.clone();
                args.extend(["--format", format]);
                quiet(
                    &exchange(&args, vec![step.clone()], Sink::Closed, backtrace, false).await,
                    &args,
                );
            }
        }
    }
}

// Break caught: a legacy println still panics instead of returning a stdout error.
#[tokio::test]
async fn closed_stdout_is_quiet_in_legacy_one_shots_and_status() {
    for (args, steps) in legacy_cases() {
        for backtrace in [false, true] {
            quiet(
                &exchange(&args, steps.clone(), Sink::Closed, backtrace, false).await,
                &args,
            );
        }
    }
}

// Break caught: helper/main swallow non-EPIPE output errors or preserve legacy panics.
#[tokio::test]
async fn non_pipe_stdout_errors_stay_visible() {
    for (args, steps) in fallible_cases()
        .into_iter()
        .map(|(a, s)| (a, vec![s]))
        .chain(legacy_cases())
    {
        for backtrace in [false, true] {
            let output = exchange(&args, steps.clone(), Sink::Full, backtrace, false).await;
            assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains("os error 28"), "{args:?}: {error}");
            assert!(!error.contains("panicked at"), "{args:?}: {error}");
            assert_eq!(
                error.contains("Stack backtrace:"),
                backtrace,
                "{args:?}: {error}"
            );
        }
    }
}

// Break caught: report's JSON output quiets a non-pipe stdout failure.
#[tokio::test]
async fn report_json_non_pipe_stdout_errors_stay_visible() {
    for backtrace in [false, true] {
        let step = rpc(
            "org.report",
            serde_json::from_str(include_str!("fixtures/atm_report/report.json")).unwrap(),
        );
        let output = exchange(
            &["report", "--format", "json"],
            vec![step],
            Sink::Full,
            backtrace,
            false,
        )
        .await;
        assert_eq!(output.status.code(), Some(1));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("os error 28"), "{error}");
        assert!(!error.contains("panicked at"), "{error}");
        assert_eq!(error.contains("Stack backtrace:"), backtrace, "{error}");
    }
}

// Break caught: matching any EPIPE (or changing SIGPIPE) hides a daemon write failure.
#[tokio::test]
async fn daemon_socket_broken_pipe_is_not_stdout_closed() {
    for (args, step, diagnostic) in [
        (
            vec!["inbox"],
            rpc("msg.inbox", inbox()),
            "daemon connection failed: Broken pipe",
        ),
        (vec!["list"], sessions(), "Failed to write to daemon"),
    ] {
        for backtrace in [false, true] {
            let output = exchange(&args, vec![step.clone()], Sink::Capture, backtrace, true).await;
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                error.contains(diagnostic) && error.contains("Broken pipe"),
                "{error}"
            );
            assert_eq!(error.contains("Stack backtrace:"), backtrace, "{error}");
        }
    }
}

// Break caught: stdout failure aborts spawn before spawn_type, or context hides the typed error.
#[tokio::test]
async fn spawn_finishes_before_reporting_stdout_failure() {
    let steps = vec![
        rpc(
            "session.spawn_prepare",
            json!({"target_pane":"%7","cwd":"/project","caller_pane_missing":false}),
        ),
        rpc("session.spawn", json!({"pane":"%42"})),
        rpc("session.spawn_type", Value::Null),
    ];
    for format in ["text", "json"] {
        for backtrace in [false, true] {
            let args = ["spawn", "--harness", "pi", "--format", format];
            quiet(
                &exchange(&args, steps.clone(), Sink::Closed, backtrace, false).await,
                &args,
            );
            let full = exchange(&args, steps.clone(), Sink::Full, backtrace, false).await;
            assert_eq!(full.status.code(), Some(1), "{full:?}");
            let error = String::from_utf8_lossy(&full.stderr);
            assert!(error.contains("Failed to print the new pane's id"));
            assert_eq!(error.contains("Stack backtrace:"), backtrace);
        }
    }
}

// Break caught: message matching quiets a daemon refusal or changes clap's syntax exit.
#[tokio::test]
async fn other_errors_keep_their_status_and_diagnostics() {
    for backtrace in [false, true] {
        let refusal = Step {
            method: "msg.inbox",
            params: None,
            reply: json!({"type":"response","id":"r1","error":{"code":"refused","message":"stdout reader closed"}}),
        };
        let output = exchange(&["inbox"], vec![refusal], Sink::Capture, backtrace, false).await;
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.starts_with("Error: stdout reader closed\n"),
            "{error}"
        );
        assert_eq!(error.contains("Stack backtrace:"), backtrace);
        let syntax = exchange(
            &["inbox", "--not-a-flag"],
            vec![],
            Sink::Capture,
            backtrace,
            false,
        )
        .await;
        assert_eq!(syntax.status.code(), Some(2), "{syntax:?}");
        assert!(String::from_utf8_lossy(&syntax.stderr).contains("unexpected argument"));
    }
}

fn paged_back() -> (Vec<Step>, Value) {
    let mut page: Value = serde_json::from_str(include_str!(
        "../crates/atm-org/tests/fixtures/return_digest_expected.json"
    ))
    .unwrap();
    page["future_digest"] = json!(true);
    page["rows"][1]["future_task"] = json!(7);
    let expected = json!({"changed":true,"away_since":"2026-11-16T08:00:00Z","provisional_count":1,"pending_count":0,"digest":page});
    let mut first = expected.clone();
    first["digest"]["rows"] = json!([page["rows"][0]]);
    first["digest"]["next_after"] = json!(0);
    let mut last = page;
    last["rows"] = json!([last["rows"][1]]);
    (
        vec![
            checked_rpc("owner.back", json!({}), first),
            checked_rpc(
                "owner.digest",
                json!({"snapshot_seq":31,"after":0,"limit":100}),
                last,
            ),
        ],
        expected,
    )
}
fn paged_overrule(changed: bool) -> (Vec<Step>, Value) {
    let mut decision = decisions()["decisions"][0].clone();
    decision["status"] = json!("overruled");
    decision["closed_at"] = json!("2026-01-15T12:01:00Z");
    decision["resolution"] =
        json!({"decision_id":"d41","option":"no","by":"owner","note":null,"standing_rule":false});
    let first = json!({"decision":decision,"changed":changed,"future_result":true,"redo":{"required_count":3,"completed_count":0,"cancelled_count":0,"task_ids":["t14"],"next_after":"t14"}});
    let last = json!({"required_count":3,"completed_count":1,"cancelled_count":0,"task_ids":["t16","t18"],"next_after":null,"future_page":true});
    let mut expected = first.clone();
    expected["redo"] = last.clone();
    expected["redo"]["task_ids"] = json!(["t14", "t16", "t18"]);
    (
        vec![
            checked_rpc(
                "decision.resolve",
                json!({"id":"d41","option":"no","note":null,"standing_rule":false}),
                first,
            ),
            checked_rpc(
                "decision.get",
                json!({"id":"d41","redo_after":"t14"}),
                json!({"decision":decision,"redo":last}),
            ),
        ],
        expected,
    )
}
fn return_and_decision_cases() -> Vec<(Vec<&'static str>, Vec<Step>)> {
    let mut withdrawn = decisions()["decisions"][0].clone();
    withdrawn["status"] = json!("withdrawn");
    withdrawn["closed_at"] = json!("2026-01-15T12:01:00Z");
    withdrawn["withdrawal"] = json!({"decision_id":"d41","by":"project/worker","reason":null});
    let mut cases = vec![
        (vec!["back"], paged_back().0),
        (vec!["decisions"], vec![rpc("decision.list", decisions())]),
        (
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
            ],
            vec![checked_rpc(
                "decision.request",
                json!({"kind":"plan","title":"Approve this plan","context":"","files":[],"options":["yes","no"],"recommended":null,"may_be_undone":true,"changes_intent":true,"deadline":null}),
                json!({"decision_id":"d41"}),
            )],
        ),
        (
            vec!["decisions", "withdraw", "d41"],
            vec![checked_rpc(
                "decision.withdraw",
                json!({"id":"d41","reason":null}),
                json!({"decision":withdrawn,"changed":true}),
            )],
        ),
    ];
    cases.extend(
        fallible_cases()
            .into_iter()
            .filter(|(args, _)| args.first() == Some(&"decide"))
            .map(|(args, step)| (args, vec![step])),
    );
    for changed in [true, false] {
        cases.push((vec!["decide", "d41", "no"], paged_overrule(changed).0));
    }
    cases
}

// Break caught: a return/decision writer bypasses the helper after collecting its pages.
#[tokio::test]
async fn closed_stdout_is_quiet_for_paged_back_and_every_decision_command() {
    for (args, steps) in return_and_decision_cases() {
        for format in ["text", "json"] {
            for backtrace in [false, true] {
                let mut args = args.clone();
                args.extend(["--format", format]);
                quiet(
                    &exchange(&args, steps.clone(), Sink::Closed, backtrace, false).await,
                    &args,
                );
            }
        }
    }
}

// Break caught: the new stdout boundaries quiet ENOSPC or abort before the last page.
#[tokio::test]
async fn non_pipe_stdout_errors_stay_visible_for_paged_back_and_every_decision_command() {
    for (args, steps) in return_and_decision_cases() {
        for format in ["text", "json"] {
            for backtrace in [false, true] {
                let mut args = args.clone();
                args.extend(["--format", format]);
                let output = exchange(&args, steps.clone(), Sink::Full, backtrace, false).await;
                assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
                let error = String::from_utf8_lossy(&output.stderr);
                assert!(error.contains("os error 28"), "{args:?}: {error}");
                assert!(!error.contains("panicked at"), "{args:?}: {error}");
                assert_eq!(
                    error.contains("Stack backtrace:"),
                    backtrace,
                    "{args:?}: {error}"
                );
            }
        }
    }
}

// Break caught: helper integration prints only the first page or alters the assembled JSON/text.
#[tokio::test]
async fn paged_back_and_overrule_keep_complete_normal_output() {
    for format in ["text", "json"] {
        for (args, (steps, expected), text) in [
            (
                vec!["back", "--format", format],
                paged_back(),
                "  redo: t14 [accepted; direct] Write cache\n",
            ),
            (
                vec!["decide", "d41", "no", "--format", format],
                paged_overrule(true),
                "d41 overruled: no\nRedo required:\n  t14\n  t16\n  t18\nThey are open and unassigned; no files were reverted.\nTasks redone for d41: 1/3 accepted again.\n",
            ),
            (
                vec!["decide", "d41", "no", "--format", format],
                paged_overrule(false),
                "d41 overruled: no (unchanged)\nRedo already recorded:\n  t14\n  t16\n  t18\nTasks redone for d41: 1/3 accepted again.\n",
            ),
        ] {
            let output = exchange(&args, steps, Sink::Capture, true, false).await;
            assert!(output.status.success() && output.stderr.is_empty(), "{args:?}: {output:?}");
            if format == "json" {
                assert_eq!(serde_json::from_slice::<Value>(&output.stdout).unwrap(), expected);
            } else if args[0] == "back" {
                assert!(String::from_utf8(output.stdout).unwrap().contains(text));
            } else {
                assert_eq!(String::from_utf8(output.stdout).unwrap(), text);
            }
        }
    }
}

// Break caught: the new boundary suppresses ordinary success or changes JSON values.
#[tokio::test]
async fn normal_stdout_still_succeeds() {
    for (args, step) in fallible_cases() {
        for format in ["text", "json"] {
            let mut args = args.clone();
            args.extend(["--format", format]);
            let expected = step.reply["result"].clone();
            let output = exchange(&args, vec![step.clone()], Sink::Capture, true, false).await;
            assert!(
                output.status.success() && output.stderr.is_empty(),
                "{args:?}: {output:?}"
            );
            assert!(!output.stdout.is_empty());
            if format == "json" {
                assert_eq!(
                    serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                    expected
                );
            }
        }
    }
    for (args, steps) in legacy_cases() {
        let output = exchange(&args, steps, Sink::Capture, true, false).await;
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "{args:?}: {output:?}"
        );
        assert!(!output.stdout.is_empty());
    }
}
