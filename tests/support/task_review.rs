//! Synthetic review probes: a future daemon, unsafe diagnostics and JSON-valued text.
use super::{case, invoke};
use serde_json::{json, Value};

// Break caught: an unknown notification is mistaken for a failed task mutation.
#[tokio::test]
async fn unknown_notification_status_is_successful_and_terminal_safe() {
    let create = case("create");
    for status in ["delivered", "\u{1b}]0;spoof\u{7}x"] {
        let reply = json!({"result":{"task_id":"t7","changed":true,"notification":{"status":status,"message_id":"m8"}}});
        for format in ["json", "text"] {
            let output = invoke(&create, &["--format", format], reply.clone()).await;
            assert!(output.status.success(), "{format} {status:?}: {output:?}");
            assert!(output.stderr.is_empty());
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
            if format == "json" {
                assert_eq!(
                    serde_json::from_str::<Value>(&text).unwrap(),
                    reply["result"]
                );
            } else {
                assert!(text.starts_with("t7 changed\n"));
                assert!(text.contains("task committed"), "{text}");
                assert!(text.contains("not understood"), "{text}");
                let safe_status = if status == "delivered" {
                    "delivered"
                } else {
                    r"\u{1b}]0;spoof\u{7}x"
                };
                assert!(text.contains(safe_status), "{text}");
            }
        }
    }
}

// Break caught: raw JSON is gated on a text renderer understanding a newer result shape.
#[tokio::test]
async fn json_output_bypasses_text_shape_validation() {
    let reply = json!({"result":{"future_result":"é\u{1b}\u{9b}"}});
    let output = invoke(&case("create"), &["--format", "json"], reply.clone()).await;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        reply["result"]
    );
    assert!(!output.stdout.contains(&0x1b));
}

// Break caught: a daemon error can write OSC/C1 controls or spoof extra stderr lines.
#[tokio::test]
async fn daemon_diagnostics_are_escaped_before_stderr() {
    let reply = json!({"error":{"code":"refused","message":"no\u{1b}]0;spoof\u{7}\nnext\u{9b}"}});
    for format in ["text", "json"] {
        let output = invoke(&case("create"), &["--format", format], reply.clone()).await;
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let text = String::from_utf8(output.stderr).unwrap();
        assert!(
            !text.chars().any(|c| c.is_control() && c != '\n'),
            "{text:?}"
        );
        assert_eq!(text.lines().count(), 1);
        assert!(
            text.contains(r"no\u{1b}]0;spoof\u{7}\nnext\u{9b}"),
            "{text}"
        );
    }
}

// Break caught: a malformed mutation answer is reported without outcome-unknown advice.
#[tokio::test]
async fn malformed_mutation_text_warns_that_the_task_may_have_committed() {
    let reply = json!({"result":{"task_id":"t7","changed":"future\u{1b}\u{9b}"}});
    let output = invoke(&case("create"), &[], reply).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(text.contains("task may have committed"), "{text:?}");
    assert!(
        text.contains("atm task get") && text.contains("atm task list"),
        "{text}"
    );
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "{text:?}"
    );
}

// Break caught: get JSON-valued text escapes a path or newline twice, breaking copy/parse.
#[tokio::test]
async fn get_details_are_single_escaped_valid_json() {
    let get = case("get");
    let mut reply = get["reply"].clone();
    reply["result"]["task"]["files"][0]["path"] = json!(r"C:\x.md");
    reply["result"]["task"]["files"][0]["note"] = json!("line1\nline2\u{9b}");
    let output = invoke(&get, &[], reply.clone()).await;
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("  files: "))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(line).unwrap(),
        reply["result"]["task"]["files"]
    );
    assert!(!text.contains('\u{9b}'));
}

// Break caught: serde silently discards misspelled file/budget keys before the daemon sees them.
#[tokio::test]
async fn file_and_budget_flags_reject_unknown_keys_before_daemon_startup() {
    let dir = tempfile::tempdir().unwrap();
    let create = ["task", "create", "New", "--measure", r#"{"kind":"owner"}"#];
    for (flag, value, field) in [
        (
            "--file",
            r#"{"path":"brief.md","kind":"brief","notes":"read first"}"#,
            "notes",
        ),
        (
            "--file",
            r#"{"path":"brief.md","kind":"brief","future":1}"#,
            "future",
        ),
        ("--budget", r#"{"turn":10}"#, "turn"),
        (
            "--budget",
            r#"{"cost":{"microdollars":100,"currency":"usd"}}"#,
            "currency",
        ),
    ] {
        let mut args = create.to_vec();
        args.extend([flag, value]);
        let output = super::support::command(dir.path(), &args, None)
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(field),
            "{output:?}"
        );
    }
    let output = super::support::command(
        dir.path(),
        &[
            "task",
            "submit",
            "t6",
            "--file",
            r#"{"path":"report.md","kind":"report","notes":"read first"}"#,
        ],
        None,
    )
    .output()
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        !dir.path().join("fallback").exists(),
        "no daemon startup for invalid keys"
    );
    assert!(!dir.path().join("rpc.sock").exists());
    assert!(!dir.path().join("state/atmd.pid").exists());
}

// Break caught: custom enum parsers hide accepted words from users reading --help.
#[tokio::test]
async fn enum_help_names_the_supported_words() {
    let dir = tempfile::tempdir().unwrap();
    for (action, phrase) in [
        (
            "create",
            "Task kind: work, review, plan, errand, drift_check",
        ),
        ("list", "Task kind: work, review, plan, errand, drift_check"),
        (
            "attach",
            "File kind: brief, report, evidence, plan, contract",
        ),
        ("submit", "Submission kind: plan, result"),
        (
            "set-state",
            "State: open, working, waiting, blocked, cancelled",
        ),
    ] {
        let output = super::support::command(dir.path(), &["task", action, "--help"], None)
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        let help = String::from_utf8(output.stdout)
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(help.contains(phrase), "{action}: {help}");
    }
    assert!(!dir.path().join("fallback").exists());
}
