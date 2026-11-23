//! Synthetic org.tree contract fixtures over a private socket; no daemon or harness.
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
    sync::oneshot,
};

const FIXTURES: [(&str, &str); 3] = [
    (
        include_str!("fixtures/atm_org/tree.json"),
        include_str!("fixtures/atm_org/tree.txt"),
    ),
    (
        include_str!("fixtures/atm_org/tree-five.json"),
        include_str!("fixtures/atm_org/tree-five.txt"),
    ),
    (
        include_str!("fixtures/atm_org/unavailable.json"),
        include_str!("fixtures/atm_org/unavailable.txt"),
    ),
];

fn tree() -> Value {
    serde_json::from_str(FIXTURES[0].0).unwrap()
}

fn command(dir: &Path, args: &[&str], pane: Option<&str>) -> Command {
    use std::os::unix::fs::PermissionsExt;
    for sub in ["home", "bin", "state", "tmux", "config", "gamma", "tmp"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    for binary in ["tmux", "atmd", "git"] {
        let path = dir.join("bin").join(binary);
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf 'fallback\\n' >> \"$ATM_ORG_FALLBACK\"\nexit 99\n",
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
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
        .env("ATM_TMUX_SOCKET", "org-cli")
        .env("ATM_AGENT", "wrong/identity")
        .env("ATM_ORG_FALLBACK", dir.join("fallback"))
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
    broken_stdout: bool,
) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("rpc.sock")).unwrap();
    let seen = Arc::new(AtomicBool::new(false));
    let observed = seen.clone();
    let expected_pane = pane.map(str::to_owned);
    let advertised = methods
        .as_ref()
        .and_then(Value::as_array)
        .is_some_and(|names| names.contains(&json!("org.tree")));
    let (exited_tx, exited_rx) = oneshot::channel();
    let peer = tokio::spawn(async move {
        // Startup may probe reachability without sending Connect.
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
        let mut hello = json!({"type":"connected", "protocol_version":{"major":1,"minor":1}, "client_id":"org-test"});
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
            let mut expected = json!({"protocol_version":{"major":1,"minor":1}, "type":"request", "id":"r1", "method":"org.tree", "params":{}});
            if let Some(pane) = expected_pane {
                expected["from_pane"] = json!(pane);
            }
            assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), expected);
            if let Some(payload) = payload {
                let mut response = json!({"type":"response", "id":"r1"});
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
        drop(write); // An absent payload deliberately exercises daemon EOF.
        assert_eq!(
            read.read_line(&mut line).await.unwrap(),
            0,
            "extra RPC: {line}"
        );
        exited_rx.await.unwrap();
        std::future::poll_fn(|cx| {
            assert!(listener.poll_accept(cx).is_pending(), "retry connection");
            std::task::Poll::Ready(())
        })
        .await;
    });
    let mut cmd = command(dir.path(), args, pane);
    if broken_stdout {
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        reader.shutdown(std::net::Shutdown::Read).unwrap();
        cmd.stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)));
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
    let _ = exited_tx.send(());
    tokio::time::timeout(Duration::from_secs(60), peer)
        .await
        .unwrap()
        .unwrap();
    assert!(!dir.path().join("fallback").exists(), "local fallback ran");
    output
}

async fn result(args: &[&str], value: Value) -> Output {
    exchange(
        args,
        Some(json!({"result": value})),
        Some(json!(["org.tree"])),
        Some("%7"),
        false,
    )
    .await
}

fn success(output: &Output) {
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

// Break caught: wrong projection/format/layout, dropped projects or cap markers.
#[tokio::test]
async fn org_matches_text_fixtures_and_raw_json() {
    for (source, expected) in FIXTURES {
        let value: Value = serde_json::from_str(source).unwrap();
        for args in [vec!["org"], vec!["org", "--format", "text"]] {
            let output = result(&args, value.clone()).await;
            success(&output);
            assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        }
        let output = result(&["org", "--format", "json"], value.clone()).await;
        success(&output);
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            value
        );
    }
}

fn department_tree() -> Value {
    let mut value = tree();
    let section: serde_json::Map<String, Value> =
        serde_json::from_str(include_str!("fixtures/atm_org/departments.json")).unwrap();
    value.as_object_mut().unwrap().extend(section);
    value
}

// Break caught: overlap duplicates project details, or text/JSON loses counts or names of problems.
#[tokio::test]
async fn org_matches_department_text_and_raw_json() {
    let value = department_tree();
    let raw = result(&["org", "--format", "json"], value.clone()).await;
    success(&raw);
    assert_eq!(serde_json::from_slice::<Value>(&raw.stdout).unwrap(), value);
    let text = result(&["org"], value).await;
    success(&text);
    let section = include_str!("fixtures/atm_org/departments.txt");
    let expected =
        FIXTURES[0]
            .1
            .replacen("\nProject alpha", &format!("\n{section}\nProject alpha"), 1);
    assert_eq!(String::from_utf8(text.stdout).unwrap(), expected);
}

// Break caught: report rows are missing, printed under the wrong department,
// allow terminal controls, or lose future fields in JSON.
#[tokio::test]
async fn org_prints_would_wait_under_its_department() {
    let mut value = department_tree();
    value["departments"][1]["would_wait"] =
        serde_json::from_str(include_str!("fixtures/atm_org/would-wait.json")).unwrap();
    let raw = result(&["org", "--format", "json"], value.clone()).await;
    success(&raw);
    assert_eq!(serde_json::from_slice::<Value>(&raw.stdout).unwrap(), value);
    let output = result(&["org"], value).await;
    success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    let (_, tools) = text.split_once("  Tools (tools)\n").unwrap();
    let (tools, _) = tools.split_once("No department:").unwrap();
    assert!(
        tools.contains("    Would wait: alpha/build-3 (roster full: 2 of 1 for alpha/build-*)\n"),
        "{text}"
    );
    assert!(
        tools.contains("    Would wait: beta/worker\\n\\u{1b} (not on roster\\r\\u{1b})\n"),
        "{text}"
    );
}

// Break caught: a failed read disappears or a config value inserts fake terminal output.
#[tokio::test]
async fn org_displays_problem_only_and_escapes_department_text() {
    let mut value = tree();
    value["departments"] = json!([]);
    value["department_problems"] = json!(["bad.toml line 3: invalid value"]);
    let output = result(&["org"], value).await;
    success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Departments:\n  none loaded\nNo department: alpha, beta, gamma\nDepartment problems:\n  bad.toml line 3: invalid value\n"));

    let mut value = department_tree();
    value["departments"][0]["title"] = json!("Apps\n\u{001b}");
    value["departments"][0]["goal"] = json!("Goal\r\u{001b}");
    value["departments"][0]["projects"] = json!(["alpha", "gamma"]);
    value["departments"][1]["problems"][0] = json!("tools.toml:projects: typo\n\u{001b}");
    value["department_problems"][0] = json!("bad.toml\n\u{001b} line 3: invalid value");
    let output = result(&["org"], value).await;
    success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    for escaped in [
        "Apps\\n\\u{1b} (apps)",
        "Goal\\r\\u{1b}",
        "typo\\n\\u{1b}",
        "bad.toml\\n\\u{1b} line 3",
    ] {
        assert!(text.contains(escaped), "{text}");
    }
    assert!(text.contains("No department: none\n"));
    assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
}

// Break caught: reserialization discards nested additions or terminal controls leak.
#[tokio::test]
async fn org_preserves_additive_fields_and_escapes_controls() {
    let mut value = tree();
    value["future"] = json!({"keep":"é\u{009b}"});
    value["future_collection"] = json!([
        {"id":"north", "projects":["alpha", "beta"]},
        {"id":"south", "projects":["alpha"]}
    ]);
    value["reporting"]["future"] = json!(true);
    value["needs_owner"]["future"] = json!([1, 2]);
    value["needs_owner"]["items"][0]["future"] = json!({"keep":null});
    value["projects"][0]["future"] = json!({"keep":"project"});
    value["projects"][0]["seats"][0]["future"] = json!({"keep":"seat"});
    value["projects"][0]["tasks"][0]["future"] = json!({"keep":"task"});
    value["projects"][0]["decisions"]["future"] = json!(9);
    value["unplaced_tasks"][0]["future"] = json!(false);
    let output = result(&["org", "--format", "json"], value.clone()).await;
    success(&output);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        value
    );
    assert!(!String::from_utf8(output.stdout)
        .unwrap()
        .contains('\u{009b}'));
    let output = result(&["org"], value).await;
    success(&output);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), FIXTURES[0].1);

    let mut value = tree();
    value["projects"][0]["tasks"][0]["title"] = json!("é🙂\nspoof\u{001b}[31m\u{007f}\u{0085}");
    let output = result(&["org"], value).await;
    success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("é🙂\\nspoof\\u{1b}[31m\\u{7f}\\u{85}"));
    assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
}

// Break caught: incorrectly requires a named pane or adds caller_server/identity params.
#[tokio::test]
async fn org_sends_one_exact_request_with_or_without_pane() {
    for pane in [None, Some("%7")] {
        let output = exchange(
            &["org"],
            Some(json!({"result":tree()})),
            Some(json!(["org.tree"])),
            pane,
            false,
        )
        .await;
        success(&output);
        assert_eq!(String::from_utf8(output.stdout).unwrap(), FIXTURES[0].1);
    }
}

// Break caught: sends unsupported RPC or falls back on an older daemon.
#[tokio::test]
async fn org_refuses_unadvertised_method_without_request() {
    for methods in [
        None,
        Some(json!([])),
        Some(json!(["agent.list", "task.list"])),
    ] {
        let output = exchange(&["org"], None, methods, Some("%7"), false).await;
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("daemon is too old"));
    }
}

// Break caught: ignores refusal/EOF or emits partial output before validating structure.
#[tokio::test]
async fn org_surfaces_remote_eof_and_invalid_results() {
    for (payload, message) in [
        (
            Some(json!({"error":{"code":"refused", "message":"org result is too large"}})),
            "org result is too large",
        ),
        (
            Some(json!({"error":{"code":"unavailable", "message":"fold unavailable"}})),
            "fold unavailable",
        ),
        (None, "daemon closed the connection"),
        (
            Some(json!({"result":{"generated_at":"now"}})),
            "invalid org tree from daemon",
        ),
        (Some(json!({"result":null})), "invalid org tree from daemon"),
    ] {
        let output = exchange(
            &["org"],
            payload,
            Some(json!(["org.tree"])),
            Some("%7"),
            false,
        )
        .await;
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{output:?}"
        );
    }
    for format in ["text", "json"] {
        let mut value = tree();
        value["projects"][0]["tasks_open"] = json!("many");
        let output = result(&["org", "--format", format], value).await;
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid org tree from daemon"));
    }
}

// Break caught: either format reports a closed reader as a noisy command failure.
#[tokio::test]
async fn org_ends_quietly_on_broken_stdout() {
    for format in ["text", "json"] {
        let output = exchange(
            &["org", "--format", format],
            Some(json!({"result":tree()})),
            Some(json!(["org.tree"])),
            Some("%7"),
            true,
        )
        .await;
        assert_eq!(output.status.code(), Some(141));
        assert!(output.stderr.is_empty(), "{output:?}");
    }
}

// Break caught: accepts identity/project overrides or initializes the TUI on help.
#[tokio::test]
async fn org_rejects_operands_and_unknown_flags() {
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("rpc.sock")).unwrap();
    for args in [
        vec!["org", "alpha"],
        vec!["org", "--project-root", "/fixtures/alpha"],
        vec!["org", "--for", "alpha/lead"],
        vec!["org", "--unknown"],
        vec!["org", "--format", "xml"],
    ] {
        let output = tokio::time::timeout(
            Duration::from_secs(60),
            command(dir.path(), &args, None).output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
    let help = tokio::time::timeout(
        Duration::from_secs(60),
        command(dir.path(), &["org", "--help"], None).output(),
    )
    .await
    .unwrap()
    .unwrap();
    success(&help);
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains("--format") && text.contains("text") && text.contains("json"));
    std::future::poll_fn(|cx| {
        assert!(
            listener.poll_accept(cx).is_pending(),
            "unexpected RPC for arguments/help"
        );
        std::task::Poll::Ready(())
    })
    .await;
    assert!(!dir.path().join("fallback").exists());
}
