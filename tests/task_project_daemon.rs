//! Synthetic projects and pi stand-ins, with real private atmd/tmux and built CLI.
//! Synchronize on committed events, readiness, model-clock gates and process exit;
//! never use the older lifecycle shell/placement polling helpers or a model CLI.
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

use atm_core::SessionStatus;
use serde_json::{json, Value};
use stand_in::{StandInMode, StandInPane};
use std::fs;
use support::{about, reference, Confirmation, MessageRig};

// Reuse the daemon-start path, but its placement window receives no test input:
// it only needs to exist for the split, not to have printed a shell prompt.
fn start_seat(rig: &mut MessageRig, project: &str, name: &str, manager: &str) -> StandInPane {
    rig.test.project = rig.test.rig.root().join(project);
    let config = lifecycle::started_config(StandInMode::Pi);
    rig.test
        .role(name, config.mode, &format!("manager='{manager}'\n"));
    let log = rig
        .test
        .next_stand_in(&format!("{project}-{name}"), &config);
    lifecycle::checked(
        rig.test
            .rig
            .tmux(&["set-option", "-g", "default-size", "200x101"]),
    );
    let placement = lifecycle::checked(rig.test.rig.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-c",
        rig.test.project.to_str().unwrap(),
        "/bin/bash --noprofile --norc",
    ]));
    let up = rig.test.up_from(name, &placement).unwrap();
    assert_eq!(up["agent_id"], format!("{project}/{name}"));
    let pane = rig
        .test
        .rig
        .observe_pane(up["pane"].as_str().unwrap().into(), config.mode, log);
    rig.test
        .rig
        .wait_event(&pane, 0, |e| e["kind"] == "rendered");
    let row = rig.test.rig.wait_for_row(&pane, |r| {
        !r.id.is_pending() && r.status == SessionStatus::Idle
    });
    rig.test.rig.wait_ready(&pane, Some(row.id.as_str()));
    pane
}

fn ids(value: &Value) -> Vec<&str> {
    value["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["task_id"].as_str().unwrap())
        .collect()
}

fn assert_views(rig: &MessageRig, alpha: &str, beta: &str, accepted: bool) {
    let a = rig.atm_json(
        None,
        &["task", "list", "--project", "alpha", "--format", "json"],
    );
    let b = rig.atm_json(
        None,
        &["task", "list", "--project", "beta", "--format", "json"],
    );
    assert_eq!(ids(&a), [alpha]);
    assert_eq!(ids(&b), [beta]);
    for (id, project) in [(alpha, "alpha"), (beta, "beta")] {
        let got = rig.atm_json(None, &["task", "get", id, "--format", "json"]);
        assert_eq!(got["task"]["project_id"], project);
        assert_eq!(got["task"]["creator"], "gamma/chief");
    }
    let report = rig.atm_json(None, &["report", "--project", "alpha", "--format", "json"]);
    assert_eq!(ids(&report), [alpha]);
    assert_eq!(report["tasks"][0]["project_id"], "alpha");
    assert_eq!(report["tasks"][0]["agent_id"], "beta/worker");
    assert_eq!(report["projects"][0]["project_id"], "alpha");
    if accepted {
        assert_eq!(report["tasks"][0]["turns"], 1);
        // The first cumulative price establishes a baseline, not historical spend.
        assert_eq!(report["tasks"][0]["cost"], json!({"microdollars":0}));
        assert_eq!(report["projects"][0]["cost"], json!({"microdollars":0}));
    }
    assert_eq!(
        report["projects"][0]["tasks"][if accepted { "accepted" } else { "open" }],
        1
    );
    let tree = rig.atm_json(None, &["org", "--format", "json"]);
    assert_eq!(tree["projects"].as_array().unwrap().len(), 3);
    assert_eq!(tree["projects"][0]["tasks_open"], usize::from(!accepted));
    assert_eq!(tree["projects"][1]["tasks_open"], 1);
    assert_eq!(tree["projects"][2]["tasks_open"], 0);
    let seat_tasks = &tree["projects"][1]["seats"][0]["tasks"];
    assert_eq!(
        seat_tasks,
        &if accepted { json!([]) } else { json!([alpha]) }
    );
    assert!(tree["unplaced_tasks"].as_array().unwrap().is_empty());
}

// Break caught: CLI selectors and daemon policy/attribution disagree across
// projects, cross-project delivery, completed work, or replay after restart.
#[test]
fn selected_task_projects_survive_cross_project_work_and_daemon_restart() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    rig.test
        .rig
        .set_daemon_env("ATM_SPAWN_BIN", rig.test.rig.launcher(StandInMode::Pi));
    let config = rig.test.rig.root().join("config/atm/config.toml");
    let mut settings = fs::read_to_string(&config).unwrap();
    settings.push_str("\n[reminder]\nenabled = false\n");
    fs::write(config, settings).unwrap();
    rig.test.rig.restart_daemon();
    assert_eq!(rig.test.rpc("agent.list", json!({}), None), json!([]));
    for project in ["alpha", "beta", "gamma"] {
        let root = rig.test.rig.root().join(project);
        fs::create_dir_all(root.join(".atm")).unwrap();
        fs::write(root.join(".atm/project.toml"), format!("id='{project}'\n")).unwrap();
        fs::write(
            root.join(".atm/intent.md"),
            if project == "gamma" {
                "# Home organisation intent\nNo measures here.\n".into()
            } else {
                format!("## How we will know\n- mechanical: {project}-check\n")
            },
        )
        .unwrap();
    }
    assert!(!rig.test.rig.root().join("gamma/.git").exists());
    let mut watch = rig.watch(); // Before registration, creation, assignment and delivery.
    let _alpha = start_seat(&mut rig, "alpha", "worker", "gamma/chief");
    let worker = start_seat(&mut rig, "beta", "worker", "gamma/chief");
    let chief = start_seat(&mut rig, "gamma", "chief", "owner");
    let brief_file = rig.test.rig.root().join("brief.md");
    let report_file = rig.test.rig.root().join("report.md");
    fs::write(&brief_file, "Alpha work, even when beta carries it out.\n").unwrap();
    fs::write(&report_file, "Result checked against alpha's measure.\n").unwrap();
    let brief_arg = json!({"path":brief_file,"kind":"brief"}).to_string();
    let report_arg = json!({"path":report_file,"kind":"report"}).to_string();
    let alpha = rig.atm_json(
        Some(&chief),
        &[
            "task",
            "create",
            "Alpha work",
            "--project",
            "alpha",
            "--measure",
            r#"{"kind":"mechanical","command":"alpha-check"}"#,
            "--file",
            &brief_arg,
            "--format",
            "json",
        ],
    );
    let alpha = alpha["task_id"].as_str().unwrap().to_owned();
    let beta = rig.atm_json(
        Some(&chief),
        &[
            "task",
            "create",
            "Beta work",
            "--project",
            "beta",
            "--measure",
            r#"{"kind":"mechanical","command":"beta-check"}"#,
            "--format",
            "json",
        ],
    );
    let beta = beta["task_id"].as_str().unwrap().to_owned();
    for (id, project) in [(&alpha, "alpha"), (&beta, "beta")] {
        let created = watch.wait("recorded target project", |e| {
            e["kind"] == "task.created" && format!("t{}", e["seq"]) == *id
        });
        assert_eq!(created["body"]["project_id"], project);
        assert_eq!(created["body"]["creator"], "gamma/chief");
    }
    let worker_cursor = rig.test.rig.event_cursor(&worker);
    let assigned = rig.atm_json(
        Some(&chief),
        &["task", "assign", &alpha, "beta/worker", "--format", "json"],
    );
    let brief = assigned["notification"]["message_id"].as_str().unwrap();
    watch.wait("confirmed beta brief", about("msg.delivered", brief));
    rig.test.rig.wait_event(&worker, worker_cursor, |e| {
        e["kind"] == "accepted" && e["text"] == reference(brief)
    });
    let inbox = rig.inbox_entry(Some(&worker), None, brief);
    assert_eq!(inbox["content"]["from"], "gamma/chief");
    assert_eq!(inbox["content"]["attachments"], json!([brief_file]));
    assert_eq!(
        fs::read_to_string(&brief_file).unwrap(),
        "Alpha work, even when beta carries it out.\n"
    );
    rig.atm_json(Some(&worker), &["msg", "ack", brief, "--format", "json"]);
    assert_views(&rig, &alpha, &beta, false);
    rig.atm_json(
        Some(&worker),
        &["task", "set-state", &alpha, "working", "--format", "json"],
    );
    rig.test.rig.wait_event(&worker, worker_cursor, |e| {
        e["kind"] == "rendered" && e["state"] == "working"
    });
    rig.finish_turn(&worker);
    rig.test
        .rig
        .wait_for_row(&worker, |r| r.status == SessionStatus::Idle);
    watch.wait("beta work usage committed to alpha task", |e| {
        e["kind"] == "task.usage_observed"
            && e["body"]["task_id"] == alpha
            && e["body"]["turn"] == false
            && e["body"]["cumulative_cost"].is_object()
    });
    let chief_cursor = rig.test.rig.event_cursor(&chief);
    let submitted = rig.atm_json(
        Some(&worker),
        &[
            "task",
            "submit",
            &alpha,
            "--file",
            &report_arg,
            "--format",
            "json",
        ],
    );
    let report = submitted["notification"]["message_id"].as_str().unwrap();
    watch.wait("confirmed chief report", about("msg.delivered", report));
    rig.test.rig.wait_event(&chief, chief_cursor, |e| {
        e["kind"] == "accepted" && e["text"] == reference(report)
    });
    let inbox = rig.inbox_entry(Some(&chief), None, report);
    assert_eq!(inbox["content"]["from"], "beta/worker");
    assert_eq!(inbox["content"]["attachments"], json!([report_file]));
    assert_eq!(
        fs::read_to_string(&report_file).unwrap(),
        "Result checked against alpha's measure.\n"
    );
    rig.atm_json(Some(&chief), &["msg", "ack", report, "--format", "json"]);
    rig.atm_json(
        Some(&chief),
        &["task", "verdict", &alpha, "accepted", "--format", "json"],
    );
    watch.wait("alpha accepted", |e| {
        e["kind"] == "task.verdict" && e["body"]["task_id"] == alpha
    });
    rig.finish_turn(&chief);
    rig.test
        .rig
        .wait_for_row(&chief, |r| r.status == SessionStatus::Idle);
    assert_views(&rig, &alpha, &beta, true);
    let cursor = rig.test.cursor();
    for args in [
        vec![
            "task",
            "create",
            "Wrong target measure",
            "--project",
            "alpha",
            "--measure",
            r#"{"kind":"mechanical","command":"beta-check"}"#,
        ],
        vec![
            "task",
            "create",
            "Home lacks measures",
            "--measure",
            r#"{"kind":"mechanical","command":"alpha-check"}"#,
        ],
    ] {
        let refused = rig.atm(Some(&chief), &args);
        assert_eq!(refused.status.code(), Some(1), "{refused:?}");
        assert!(refused.stdout.is_empty());
    }
    assert!(!rig
        .test
        .tail(cursor)
        .iter()
        .any(|e| e["kind"] == "task.created"));
    watch.catch_up(&rig);
    assert_eq!(watch.all(|e| e["kind"] == "task.created").len(), 2);
    // Replay does not re-run current intent admission or silently infer a new project.
    fs::write(
        rig.test.rig.root().join("alpha/.atm/intent.md"),
        "# Changed policy without measures\n",
    )
    .unwrap();
    rig.test.rig.restart_daemon();
    rig.test.rpc("agent.list", json!({}), None); // Initial reconciliation barrier.
    assert_views(&rig, &alpha, &beta, true);
    rig.test.rig.close();
}
