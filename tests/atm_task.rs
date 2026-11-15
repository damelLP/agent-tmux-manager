//! Task CLI contracts over private sockets and recorded real method answers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};
#[path = "support/task_cli.rs"]
mod support;

// Break caught: a command missing from discoverable CLI, before any socket call.
#[tokio::test]
async fn all_nine_task_commands_have_help() {
    let dir = tempfile::tempdir().unwrap();
    for action in [
        "create",
        "assign",
        "set-state",
        "submit",
        "attach",
        "review",
        "verdict",
        "list",
        "get",
    ] {
        let output = support::command(dir.path(), &["task", action, "--help"], None)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{action}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("--format"));
    }
    assert!(!dir.path().join("fallback").exists());
}

// Break caught: CLI fixtures drift from real method results or lose refusal messages.
// Captured: atmd b3f75b26, 2026-11-15, private DaemonServer with committed synthetic seed state;
// no real harness payloads or model turns. Seed clock is 2026-01-15T12:00:00Z.
#[tokio::test]
async fn method_answers_match_the_recorded_contract() {
    let fixtures = support::contracts();
    assert_eq!(fixtures.len(), 18);
    for fixture in fixtures {
        let answer = support::method_answer(&fixture, None).await;
        let name = fixture["name"].as_str().unwrap();
        let expected = if let Some(name) = name.strip_suffix("-refusal") {
            refusals()[name].clone()
        } else {
            case(name.strip_suffix("-success").unwrap())["reply"].clone()
        };
        assert_eq!(answer, expected, "{name}");
    }
}

fn cases() -> Vec<Value> {
    serde_json::from_str(include_str!("fixtures/atm_task/cases.json")).unwrap()
}
fn case(name: &str) -> Value {
    cases()
        .into_iter()
        .find(|case| case["name"] == name)
        .unwrap()
}
fn refusals() -> Value {
    serde_json::from_str(include_str!("fixtures/atm_task/refusals.json")).unwrap()
}
fn args(case: &Value) -> Vec<&str> {
    case["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap())
        .collect()
}
async fn invoke(case: &Value, extra: &[&str], reply: Value) -> std::process::Output {
    let mut args = args(case);
    args.extend_from_slice(extra);
    support::exchange(
        &args,
        case["method"].as_str().unwrap(),
        case["params"].clone(),
        reply,
        Some("%7"),
        true,
        false,
    )
    .await
}

// Break caught: wrong method/default parameters, dropped output fields, false notification receipt.
#[tokio::test]
async fn every_method_renders_recorded_text_and_preserves_raw_json() {
    for case in cases() {
        for extra in [vec![], vec!["--format", "text"]] {
            let output = invoke(&case, &extra, case["reply"].clone()).await;
            assert!(output.status.success(), "{}: {output:?}", case["name"]);
            assert!(output.stderr.is_empty());
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                case["text"].as_str().unwrap()
            );
        }
        // Synthetic: a newer daemon adds fields that typed decoding must not erase.
        let mut reply = case["reply"].clone();
        reply["result"]["future"] = json!({"keep":"é\u{009b}","integer":9007199254740993_u64});
        let output = invoke(&case, &["--format", "json"], reply.clone()).await;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            reply["result"]
        );
        assert!(!String::from_utf8(output.stdout)
            .unwrap()
            .contains('\u{009b}'));
    }
}

// Break caught: daemon refusal text is hidden/reworded, exits successfully or emits success output.
#[tokio::test]
async fn every_method_passes_recorded_refusals_through() {
    for case in cases() {
        for format in ["text", "json"] {
            let error = refusals()[case["name"].as_str().unwrap()].clone();
            let output = invoke(&case, &["--format", format], error.clone()).await;
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            assert!(output.stdout.is_empty());
            assert_eq!(
                String::from_utf8(output.stderr).unwrap(),
                format!("Error: {}\n", error["error"]["message"].as_str().unwrap())
            );
        }
    }
}

// Break caught: optional flags lost, dollars rounded, files conflated, or creator/by guessed locally.
#[tokio::test]
async fn optional_arguments_keep_existing_structured_shapes() {
    let tests = [
        (
            vec![
                "task",
                "create",
                "Review é",
                "--kind",
                "review",
                "--parent",
                "t1",
                "--judges",
                "t2",
                "--measure",
                r#"{"kind":"reviewed","rubric":"rubric.md"}"#,
                "--budget",
                r#"{"cost":{"microdollars":9007199254740993},"turns":12,"deadline":"2026-12-01T00:00:00Z"}"#,
                "--file",
                r#"{"path":"brief.md","kind":"brief","note":"one"}"#,
                "--file",
                r#"{"path":"contract.md","kind":"contract","note":"two"}"#,
                "--rests-on",
                "d3",
            ],
            "task.create",
            json!({"title":"Review é","kind":"review","parent":"t1","judges":"t2","measure":{"kind":"reviewed","rubric":"rubric.md"},"budget":{"cost":{"microdollars":9007199254740993_i64},"turns":12,"deadline":"2026-12-01T00:00:00Z"},"files":[{"path":"brief.md","kind":"brief","note":"one"},{"path":"contract.md","kind":"contract","note":"two"}],"rests_on":"d3"}),
        ),
        (
            vec![
                "task",
                "create",
                "Check",
                "--measure",
                r#"{"kind":"mechanical","command":"check é\nnext"}"#,
            ],
            "task.create",
            json!({"title":"Check","kind":"work","parent":null,"judges":null,"measure":{"kind":"mechanical","command":"check é\nnext"},"budget":null,"files":[],"rests_on":null}),
        ),
        (
            vec![
                "task",
                "set-state",
                "t6",
                "waiting",
                "--reason",
                "needs\nanswer",
            ],
            "task.set_state",
            json!({"task_id":"t6","state":"waiting","reason":"needs\nanswer"}),
        ),
        (
            vec![
                "task",
                "submit",
                "t6",
                "--what",
                "plan",
                "--file",
                r#"{"path":"plan.md","kind":"plan","note":"review this"}"#,
            ],
            "task.submit",
            json!({"task_id":"t6","what":"plan","files":[{"path":"plan.md","kind":"plan","note":"review this"}]}),
        ),
        (
            vec![
                "task",
                "attach",
                "t6",
                "contract.md",
                "--kind",
                "contract",
                "--note",
                "é\nnotes",
            ],
            "task.attach",
            json!({"task_id":"t6","file":{"path":"contract.md","kind":"contract","note":"é\nnotes"}}),
        ),
        (
            vec![
                "task",
                "verdict",
                "t6",
                "changes_requested",
                "--review-task",
                "t9",
                "--note",
                "fix it",
            ],
            "task.verdict",
            json!({"task_id":"t6","verdict":"changes_requested","review_task":"t9","note":"fix it"}),
        ),
        (
            vec![
                "task",
                "list",
                "--agent",
                "project/worker",
                "--creator",
                "owner",
                "--state",
                "waiting",
                "--kind",
                "plan",
                "--parent",
                "t1",
                "--after",
                "t0",
                "--open",
                "--limit",
                "1000",
            ],
            "task.list",
            json!({"agent_id":"project/worker","creator":"owner","state":"waiting","kind":"plan","parent":"t1","after":"t0","open_only":true,"limit":1000}),
        ),
        (
            vec![
                "task",
                "--format",
                "json",
                "list",
                "--creator",
                "project/lead",
                "--limit",
                "1",
            ],
            "task.list",
            json!({"agent_id":null,"creator":"project/lead","state":null,"kind":null,"parent":null,"after":null,"open_only":false,"limit":1}),
        ),
    ];
    for (args, method, params) in tests {
        let reply = if method == "task.list" {
            json!({"result":{"tasks":[],"next_after":null}})
        } else {
            json!({"result":{"task_id":"t6","changed":true,"notification":null}})
        };
        let output = support::exchange(&args, method, params, reply, None, true, false).await;
        assert!(output.status.success(), "{args:?}: {output:?}");
    }
}

// Break caught: a no-op claims change, or a committed task with unconfirmed notification is concealed.
#[tokio::test]
async fn noops_and_unconfirmed_notifications_are_not_false_success_claims() {
    let case = case("assign");
    let output = invoke(
        &case,
        &[],
        json!({"result":{"task_id":"t6","changed":false,"notification":null}}),
    )
    .await;
    assert_eq!(output.stdout, b"t6 unchanged\n");
    // Synthetic: the task write succeeded but the following message enqueue did not confirm.
    let reply = json!({"result":{"task_id":"t6","changed":true,"notification":{"status":"unconfirmed","error":{"code":"unavailable","message":"task t6 committed; inspect the task and inbox before resending"}}}});
    for format in ["text", "json"] {
        let output = invoke(&case, &["--format", format], reply.clone()).await;
        assert!(output.status.success(), "the task committed: {output:?}");
        if format == "text" {
            assert_eq!(output.stdout, b"t6 changed\nnotification: unconfirmed unavailable: task t6 committed; inspect the task and inbox before resending\n");
        } else {
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                reply["result"]
            );
        }
    }
}

// Break caught: empty/cursor pages hidden, controls spoof rows, or cost/new fields omitted by get.
#[tokio::test]
async fn lists_gets_controls_pagination_and_cost_are_visible_and_safe() {
    let list = case("list");
    let output = invoke(&list, &[], json!({"result":{"tasks":[],"next_after":"t6"}})).await;
    assert_eq!(output.stdout, b"No matching tasks.\nnext: --after t6\n");
    let mut reply = list["reply"].clone();
    reply["result"]["tasks"][0]["title"] =
        json!("é🙂\\file\nspoof\u{001b}\u{0007}\u{009b}\u{007f}");
    reply["result"]["tasks"][0]["reason"] = json!("needs\tanswer\rnow");
    reply["result"]["tasks"][0]["agent_id"] = json!("project/worker");
    let output = invoke(&list, &[], reply).await;
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("agent=project/worker\n"));
    assert!(
        text.contains("é🙂\\\\file\\nspoof\\u{1b}\\u{7}\\u{9b}\\u{7f}"),
        "{text}"
    );
    assert!(text.contains("reason: needs\\tanswer\\rnow"));
    assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
    let get = case("get");
    let mut reply = get["reply"].clone();
    reply["result"]["task"]["cost"] = json!({"turns":5,"review_rounds":2,"seconds_in_state":{"working":10},"cost":{"microdollars":9007199254740993_u64},"peak_context_percent":40});
    reply["result"]["task"]["future\u{001b}"] = json!({"note":"é\u{009b}"});
    let output = invoke(&get, &[], reply).await;
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("9007199254740993"));
    assert!(text.contains("\"seconds_in_state\":{\"working\":10}"));
    assert!(text.contains("future\\u{1b}:"));
    assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
}

// Break caught: unsupported/malformed method answers rendered as success or stdout failures swallowed.
#[tokio::test]
async fn old_daemons_bad_results_and_failed_writes_are_errors() {
    for name in ["create", "list", "get"] {
        let case = case(name);
        for (reply, advertised, diagnostic) in [
            (json!({}), false, "daemon is too old"),
            (
                json!({"result":{}}),
                true,
                "invalid task result from daemon",
            ),
        ] {
            let output = support::exchange(
                &args(&case),
                case["method"].as_str().unwrap(),
                case["params"].clone(),
                reply,
                Some("%7"),
                advertised,
                false,
            )
            .await;
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            assert!(output.stdout.is_empty());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(diagnostic),
                "{output:?}"
            );
        }
        for format in ["text", "json"] {
            let mut args = args(&case);
            args.extend(["--format", format]);
            let output = support::exchange(
                &args,
                case["method"].as_str().unwrap(),
                case["params"].clone(),
                case["reply"].clone(),
                None,
                true,
                true,
            )
            .await;
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("No space left on device"));
        }
    }
}

// Break caught: syntax/identity override accepted, or malformed task IDs use legacy free-text parsing.
#[tokio::test]
async fn bad_arguments_exit_two_without_contacting_a_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let mut bad = vec![
        vec!["task"],
        vec!["task", "create", "New"],
        vec!["task", "assign", "t6"],
        vec!["task", "assign", "t6", "worker"],
        vec!["task", "set-state", "t6"],
        vec!["task", "attach", "t6", "x"],
        vec!["task", "review", "t6"],
        vec!["task", "verdict", "t6"],
        vec!["task", "get"],
        vec!["task", "list", "--format", "xml"],
        vec!["task", "list", "--limit", "0"],
        vec!["task", "list", "--limit", "1001"],
        vec!["task", "list", "--agent", "worker"],
        vec!["task", "list", "--creator", "lead"],
        vec!["task", "get", "t6", "--from", "owner"],
        vec!["task", "get", "t6", "--lead"],
        vec!["task", "create", "New", "--measure", "not-json"],
        vec![
            "task",
            "create",
            "New",
            "--measure",
            r#"{"kind":"mechanical"}"#,
        ],
        vec![
            "task",
            "create",
            "New",
            "--measure",
            r#"{"kind":"owner"}"#,
            "--budget",
            r#"{"cost":1.5}"#,
        ],
        vec!["task", "submit", "t6", "--file", r#"{"kind":"report"}"#],
    ];
    for id in ["t01", "t-1", "t+1", "work", "t18446744073709551616", "t1\n"] {
        bad.push(vec!["task", "get", id]);
    }
    for args in bad {
        let output = support::command(dir.path(), &args, None)
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    }
    assert!(!dir.path().join("fallback").exists());
}

// Break caught: scripted peers mask client/server schema or caller-attribution drift.
#[tokio::test]
async fn every_task_command_works_against_the_real_private_daemon() {
    let contracts = support::contracts();
    for case in cases() {
        let name = format!("{}-success", case["name"].as_str().unwrap());
        let fixture = contracts
            .iter()
            .find(|fixture| fixture["name"] == name)
            .unwrap();
        let mut args = args(&case);
        args.extend(["--format", "json"]);
        assert_eq!(
            support::method_answer(fixture, Some(&args)).await,
            case["reply"],
            "{name}"
        );
    }
}

// Break caught: newer enum words cause a failed read, disappear in JSON, or hide cost details.
#[tokio::test]
async fn newer_task_enum_words_remain_readable_in_both_formats() {
    for name in ["list", "get"] {
        let case = case(name);
        // Synthetic: later task vocabularies, not malformed known values.
        let mut reply = case["reply"].clone();
        let task = if name == "list" {
            &mut reply["result"]["tasks"][0]
        } else {
            &mut reply["result"]["task"]
        };
        task["kind"] = json!("future_kind");
        task["state"] = json!("future_state");
        if name == "get" {
            task["files"][0]["kind"] = json!("future_file");
            task["measure"] = json!({"kind":"future_measure","data":42});
            task["submission"] = json!("future_submission");
            task["cost"] = json!({"seconds_in_state":{"future_state":9},"future_metric":17});
            task["last_verdict"] = json!("future_verdict");
        }
        let output = invoke(&case, &[], reply.clone()).await;
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("t6 unknown kind=unknown"), "{text}");
        if name == "get" {
            for word in [
                "future_file",
                "future_measure",
                "future_submission",
                "future_state",
                "future_metric",
                "future_verdict",
            ] {
                assert!(text.contains(word), "{word}: {text}");
            }
        }
        let output = invoke(&case, &["--format", "json"], reply.clone()).await;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            reply["result"]
        );
    }
}

// Break caught: CLI mistakes replay compatibility for permission to create newer task values.
#[tokio::test]
async fn newer_enum_inputs_reach_daemon_field_refusals() {
    let tests: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/atm_task/unknown-inputs.json")).unwrap();
    let contracts = support::contracts();
    for test in tests {
        let name = test["name"].as_str().unwrap();
        let key = test["key"].as_str().unwrap();
        let case = case(name);
        let fixture_name = format!("{name}-success");
        let mut fixture = contracts
            .iter()
            .find(|fixture| fixture["name"] == fixture_name)
            .unwrap()
            .clone();
        fixture["request"]["params"] = case["params"].clone();
        fixture["request"]["params"][key] = test["value"].clone();
        assert_eq!(
            support::method_answer(&fixture, None).await,
            test["reply"],
            "{name} {key}"
        );
        let output = support::exchange(
            &args(&test),
            case["method"].as_str().unwrap(),
            fixture["request"]["params"].clone(),
            test["reply"].clone(),
            fixture["request"]["from_pane"].as_str(),
            true,
            false,
        )
        .await;
        assert_eq!(output.status.code(), Some(1), "{name} {key}: {output:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!(
                "Error: {}\n",
                test["reply"]["error"]["message"].as_str().unwrap()
            )
        );
    }
}
