//! Synthetic org.report contract over private scripted sockets; no harness or tmux.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use serde_json::{json, Value};
use std::{
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
const SOURCE: &str = include_str!("fixtures/atm_report/report.json");
const TEXT: &str = include_str!("fixtures/atm_report/report.txt");
fn fixture() -> Value {
    serde_json::from_str(SOURCE).unwrap()
}
fn default_params() -> Value {
    json!({"project_id":null,"since":null,"until":null,"bucket":5,"after":null,"limit":100})
}
fn command(dir: &Path, args: &[&str], pane: Option<&str>) -> Command {
    use std::os::unix::fs::PermissionsExt;
    for sub in ["home", "bin", "state", "tmux", "config", "gamma", "tmp"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    for bin in ["atmd", "tmux", "git"] {
        let p = dir.join("bin").join(bin);
        std::fs::write(
            &p,
            "#!/bin/sh\nprintf fallback >> \"$ATM_REPORT_FALLBACK\"\nexit 99\n",
        )
        .unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert!(!dir.join("gamma/.git").exists());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atm"));
    cmd.env_clear()
        .args(args)
        .current_dir(dir.join("gamma"))
        .env("HOME", dir.join("home"))
        .env("SHELL", "/bin/sh")
        .env("ATM_SOCKET", dir.join("rpc.sock"))
        .env("ATM_STATE_DIR", dir.join("state"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("TMPDIR", dir.join("tmp"))
        .env("TMUX_TMPDIR", dir.join("tmux"))
        .env("ATM_TMUX_SOCKET", "report-cli")
        .env("ATM_AGENT", "wrong/identity")
        .env("ATM_REPORT_FALLBACK", dir.join("fallback"))
        .env("PATH", dir.join("bin"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(pane) = pane {
        cmd.env("TMUX_PANE", pane);
    }
    cmd
}
async fn exchange(
    args: &[&str],
    payload: Option<Value>,
    methods: Option<Value>,
    pane: Option<&str>,
    params: Value,
    broken_stdout: bool,
) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("rpc.sock")).unwrap();
    let expected_pane = pane.map(str::to_owned);
    let advertised = methods
        .as_ref()
        .and_then(Value::as_array)
        .is_some_and(|m| m.contains(&json!("org.report")));
    let (exited_tx, exited_rx) = oneshot::channel();
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
            break (read, write);
        };
        let mut hello = json!({"type":"connected","protocol_version":{"major":1,"minor":1},"client_id":"report-fixture"});
        if let Some(methods) = methods {
            hello["methods"] = methods;
        }
        write
            .write_all(format!("{hello}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        if advertised {
            assert_ne!(read.read_line(&mut line).await.unwrap(), 0);
            let mut expected = json!({"protocol_version":{"major":1,"minor":1},"type":"request","id":"r1","method":"org.report","params":params});
            if let Some(pane) = expected_pane {
                expected["from_pane"] = json!(pane);
            }
            assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), expected);
            if let Some(payload) = payload {
                let mut response = json!({"type":"response","id":"r1"});
                response
                    .as_object_mut()
                    .unwrap()
                    .extend(payload.as_object().unwrap().clone());
                write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
            line.clear();
        }
        drop(write);
        assert_eq!(
            read.read_line(&mut line).await.unwrap(),
            0,
            "extra RPC {line}"
        );
        exited_rx.await.unwrap();
        std::future::poll_fn(|cx| {
            assert!(listener.poll_accept(cx).is_pending(), "retry");
            std::task::Poll::Ready(())
        })
        .await;
    });
    let mut cmd = command(dir.path(), args, pane);
    if broken_stdout {
        let (read, write) = std::os::unix::net::UnixStream::pair().unwrap();
        read.shutdown(std::net::Shutdown::Read).unwrap();
        cmd.stdout(Stdio::from(std::os::fd::OwnedFd::from(write)));
    }
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        cmd.spawn().unwrap().wait_with_output(),
    )
    .await
    .unwrap()
    .unwrap();
    let _ = exited_tx.send(());
    if output.status.code() == Some(2) {
        peer.abort();
        panic!(
            "missing command/invalid argument: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    tokio::time::timeout(Duration::from_secs(60), peer)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !dir.path().join("fallback").exists(),
        "fallback command ran"
    );
    output
}
async fn result(args: &[&str], value: Value) -> Output {
    exchange(
        args,
        Some(json!({"result":value})),
        Some(json!(["org.report"])),
        Some("%7"),
        default_params(),
        false,
    )
    .await
}
fn success(output: &Output) {
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

// Break caught: no command, wrong params, client-side aggregation or changed text/JSON contract.
#[tokio::test]
async fn report_matches_the_text_fixture_and_preserves_raw_json() {
    for args in [vec!["report"], vec!["report", "--format", "text"]] {
        let out = result(&args, fixture()).await;
        success(&out);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), TEXT);
    }
    let mut value = fixture();
    value["future"] = json!({"keep":"é\u{009b}"});
    value["tasks"][0]["future"] = json!([1, 2]);
    value["projects"][0]["future"] = json!(true);
    let out = result(&["report", "-f", "json"], value.clone()).await;
    success(&out);
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), value);
    assert!(!String::from_utf8(out.stdout).unwrap().contains('\u{009b}'));
}
// Break caught: caller identity/cwd determines a local project or an unbound caller is refused.
#[tokio::test]
async fn report_sends_exact_filters_with_or_without_a_named_pane() {
    for pane in [None, Some("%7")] {
        let p = json!({"project_id":"alpha","since":"2026-01-01T00:00:00Z","until":"2026-01-02T00:00:00Z","bucket":2,"after":null,"limit":100});
        let out = exchange(
            &[
                "report",
                "--project",
                "alpha",
                "--since",
                "2026-01-01T00:00:00Z",
                "--until",
                "2026-01-02T00:00:00Z",
                "--bucket",
                "2",
                "-f",
                "json",
            ],
            Some(json!({"result":fixture()})),
            Some(json!(["org.report"])),
            pane,
            p,
            false,
        )
        .await;
        success(&out);
    }
}
// Break caught: missing capability triggers unsupported writes or an assembled fallback report.
#[tokio::test]
async fn report_refuses_old_daemons_without_request_or_fallback() {
    for methods in [
        None,
        Some(json!([])),
        Some(json!(["org.tree", "task.list"])),
    ] {
        let out = exchange(&["report"], None, methods, None, default_params(), false).await;
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("daemon is too old"));
    }
}
// Break caught: malformed/oversized/failed responses become successful partial output.
#[tokio::test]
async fn report_surfaces_remote_errors_eof_and_invalid_data_before_output() {
    for (payload, want) in [
        (
            Some(json!({"error":{"code":"refused","message":"report exceeds byte budget"}})),
            "report exceeds byte budget",
        ),
        (None, "closed"),
        (Some(json!({"result":{}})), "invalid report"),
        (Some(json!({"result":{"seq":"bad"}})), "invalid report"),
    ] {
        let out = exchange(
            &["report"],
            payload,
            Some(json!(["org.report"])),
            None,
            default_params(),
            false,
        )
        .await;
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(want),
            "{out:?}"
        );
    }
}
// Break caught: terminal control injection or a silently hidden page cursor.
#[tokio::test]
async fn report_escapes_controls_and_labels_the_daemons_task_page() {
    let mut value = fixture();
    value["tasks"][0]["task_id"] = json!("t5\nspoof\u{001b}");
    value["tasks"][0]["cost_source"]["models"] = json!(["model\nspoof\u{001b}"]);
    value["next_after"] = json!("t5\nspoof\u{001b}");
    let out = result(&["report"], value).await;
    success(&out);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
    assert!(text.contains("t5\\nspoof\\u{1b}"));
    assert!(text.contains("next_after"));
    assert!(text.contains("whole scope"));
}
// Break caught: report bypasses the common quiet closed-stdout policy.
#[tokio::test]
async fn report_exits_quietly_when_stdout_closes() {
    for format in ["text", "json"] {
        let out = exchange(
            &["report", "-f", format],
            Some(json!({"result":fixture()})),
            Some(json!(["org.report"])),
            None,
            default_params(),
            true,
        )
        .await;
        assert_eq!(out.status.code(), Some(141));
        assert!(out.stderr.is_empty());
    }
}
// Break caught: empty/unknown reports invent zeros or infer totals from the task page.
#[tokio::test]
async fn report_preserves_empty_unknown_and_whole_scope_totals() {
    let mut value = fixture();
    value["tasks"] = json!([]);
    value["projects"] = json!([]);
    let out = result(&["report"], value).await;
    success(&out);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("Total  0 accepted  0 cancelled  0 open  0 unknown"));
    assert!(text.contains("unknown, 0 tasks unknown"));
    assert!(!text.contains("$0.00"));
    let mut value = fixture();
    value["tasks"].as_array_mut().unwrap().truncate(1);
    value["next_after"] = json!("t5");
    for key in [
        "turns",
        "review_rounds",
        "changes_requested",
        "peak_context_percent",
        "context_percent_at_submit",
        "cost",
    ] {
        value["tasks"][0][key] = Value::Null;
    }
    let out = result(&["report"], value).await;
    success(&out);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("Total  1 accepted  0 cancelled  1 open"));
    assert!(text.contains("turns -  reviews -  rework -"));
    assert!(text.contains("context unknown peak, unknown at submit  unknown"));
    assert!(text.contains("next_after t5"));
}
fn question(source: &str) -> Value {
    let mut value: Value = serde_json::from_str(source).unwrap();
    value["seq"] = json!(123);
    value
}
// Break caught: known trend medians/directions are replaced by unknowns or a composite score.
#[tokio::test]
async fn report_renders_known_lower_medians_and_each_direction() {
    let value = question(include_str!(
        "../crates/atm-org/tests/fixtures/report/q6-change-trend.json"
    ));
    let out = result(&["report"], value).await;
    success(&out);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("trend per 2 tasks, not size-adjusted   turns 9→14 up   peak context 41→62% up   reviews 1→1 flat   rework 0→1 up   cost $1.10→$1.90 up"));
}
// Break caught: positive/unknown budget overage is dropped or presented as actual savings.
#[tokio::test]
async fn report_renders_budget_components_as_an_upper_bound() {
    let mut value = question(include_str!(
        "../crates/atm-org/tests/fixtures/report/q8-budget-overage.json"
    ));
    let out = result(&["report"], value.clone()).await;
    success(&out);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("over budget turns 1, cost $0.20, deadline 2s"));
    assert!(text.contains("unplaced  0 accepted  0 cancelled  1 open"));
    assert!(text.contains("budget savings are an upper bound"));
    value["tasks"][0]["over_budget"] = json!({"turns":null,"cost":null,"deadline_seconds":null});
    let out = result(&["report"], value).await;
    success(&out);
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("over budget unknown"));
}
// Break caught: task-level step-in kinds disappear behind only a total.
#[tokio::test]
async fn report_displays_each_nonzero_owner_step_in_kind() {
    let value = question(include_str!(
        "../crates/atm-org/tests/fixtures/report/q4-owner-step-ins.json"
    ));
    let out = result(&["report"], value).await;
    success(&out);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("you 6 (verdict 1, redirect 3, answer 1, message 1)"));
    assert!(!text.contains("PRIVATE_"));
}

// Break caught: invalid CLI inputs start/contact the daemon before being rejected.
#[tokio::test]
async fn report_rejects_invalid_inputs_without_connecting() {
    for args in [
        vec!["report", "--bucket", "0"],
        vec!["report", "--bucket", "1001"],
        vec!["report", "--format", "xml"],
        vec!["report", "--after", "t5"],
        vec!["report", "operand"],
        vec!["report", "--since", "invalid"],
        vec!["report", "--since", "999999999999999999999d"],
        vec![
            "report",
            "--since",
            "2026-02-01T00:00:00Z",
            "--until",
            "2026-01-01T00:00:00Z",
        ],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("rpc.sock")).unwrap();
        let out = tokio::time::timeout(
            Duration::from_secs(60),
            command(dir.path(), &args, None).output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!out.status.success(), "{args:?}");
        assert!(out.stdout.is_empty());
        std::future::poll_fn(|cx| {
            assert!(
                listener.poll_accept(cx).is_pending(),
                "invalid input connected: {args:?}"
            );
            std::task::Poll::Ready(())
        })
        .await;
        assert!(!dir.path().join("fallback").exists());
    }
}
