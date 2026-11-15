//! Budget checks on a sealed real daemon; scripted stand-ins, no real models.
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

use serde_json::{json, Value};
use support::{Confirmation, MessageRig};

fn start_without_reminders() -> MessageRig {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let config = rig.test.rig.root().join("config/atm/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("[reminder]\nenabled=false\n");
    std::fs::write(&config, text).unwrap();
    rig.test.rig.restart_daemon();
    assert_eq!(rig.call("agent.list", json!({}), None).unwrap(), json!([]));
    assert!(!rig.daemon_log().contains("Invalid reminder configuration"));
    rig
}

fn create_due(rig: &MessageRig) -> String {
    rig.call(
        "task.create",
        json!({
            "title":"Deadline work", "measure":{"kind":"owner"},
            "budget":{"deadline":"2000-01-01T00:00:00Z"}
        }),
        None,
    )
    .unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn budget_note(event: &Value, task: &str) -> bool {
    event["kind"] == "msg.sent"
        && event["body"]["task"] == task
        && event["body"]["from"] == "system"
        && event["body"]["to"] == "owner"
        && event["body"]["body"]
            .as_str()
            .is_some_and(|body| body.contains("crossed a budget limit"))
}

// Break caught: [reminder] enabled=false disables the only budget scheduler,
// or a restart/read clears its ordinary msg.sent latch and duplicates the note.
#[test]
fn reminders_off_still_notifies_due_budget_once_across_restart() {
    let mut rig = start_without_reminders();
    let mut watch = rig.watch();
    let task = create_due(&rig);
    let sent = watch.wait("budget note with reminders off", |event| {
        budget_note(event, &task)
    });
    let message = format!("m{}", sent["seq"].as_u64().unwrap());
    let got = rig.call("task.get", json!({"task_id":task}), None).unwrap();
    assert_eq!(got["task"]["state"], "open");
    assert_eq!(got["budget_status"]["crossed"], json!(["deadline"]));
    assert_eq!(got["budget_status"]["notification"], message);
    // The canonical note tells its recipient to run this exact CLI command.
    // Check both client views, without comparing advancing usage wall time.
    let output = rig.atm(None, &["task", "get", &task]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let displayed = text
        .lines()
        .find_map(|line| line.strip_prefix("  budget_status: "))
        .map(|value| serde_json::from_str::<Value>(value).unwrap());
    assert_eq!(displayed.as_ref(), Some(&got["budget_status"]));
    let raw = rig.atm_json(None, &["task", "get", &task, "--format", "json"]);
    assert_eq!(raw["budget_status"], got["budget_status"]);
    rig.call("msg.ack", json!({"ids":[message]}), None).unwrap();
    rig.test.rig.restart_daemon();

    let mut after = rig.watch();
    // This second budget note fences a completed post-restart budget pass;
    // it is not a fixed delay used to assert absence of the first task's note.
    let other = create_due(&rig);
    after.wait("post-restart budget pass", |event| {
        budget_note(event, &other)
    });
    let notes = rig
        .test
        .tail(0)
        .into_iter()
        .filter(|event| budget_note(event, &task))
        .count();
    assert_eq!(notes, 1);
    let got_after = rig.call("task.get", json!({"task_id":task}), None).unwrap();
    // T4a's running usage includes elapsed wall time, which may advance.
    assert_eq!(got_after["task"], got["task"]);
    assert_eq!(got_after["budget_status"], got["budget_status"]);
    assert!(!rig
        .test
        .tail(0)
        .iter()
        .any(|event| event["kind"] == "task.reminded"));
    rig.test.rig.close();
}

// Break caught: the real prompt-ingestion boundary never advances the task
// ledger, or the normal runtime cannot deliver its budget note to the creator.
#[test]
fn committed_stand_in_prompt_notifies_the_creator_without_stopping_work() {
    let mut rig = start_without_reminders();
    let lead = rig.ready_agent("lead", stand_in::StandInMode::Pi);
    let worker = rig.ready_agent("worker", stand_in::StandInMode::Pi);
    let cursor = rig.test.rig.event_cursor(&lead.pane);
    let mut watch = rig.watch();
    let task = rig
        .call(
            "task.create",
            json!({
                "title":"One observed turn", "measure":{"kind":"owner"}, "budget":{"turns":0}
            }),
            Some(&lead.pane),
        )
        .unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let assigned = rig
        .call(
            "task.assign",
            json!({"task_id":task,"agent_id":worker.id}),
            Some(&lead.pane),
        )
        .unwrap();
    let brief = assigned["notification"]["message_id"].as_str().unwrap();
    let sent = watch.wait("budget note to the task creator", |event| {
        event["kind"] == "msg.sent"
            && event["body"]["task"] == task
            && event["body"]["from"] == "system"
            && event["body"]["to"] == lead.id
            && event["body"]["body"]
                .as_str()
                .is_some_and(|body| body.contains("crossed a budget limit"))
    });
    let message = format!("m{}", sent["seq"].as_u64().unwrap());
    watch.wait(
        "budget note accepted by the creator",
        support::about("msg.delivered", &message),
    );
    // The prompt's usage event can queue the budget note before the message
    // runtime records delivery of that very prompt. Accept either ordering.
    if watch.all(support::about("msg.delivered", brief)).is_empty() {
        watch.wait(
            "assigned brief accepted by stand-in",
            support::about("msg.delivered", brief),
        );
    }
    assert_eq!(
        rig.accepted_since(&lead.pane, cursor),
        [support::reference(&message)]
    );
    let got = rig.call("task.get", json!({"task_id":task}), None).unwrap();
    assert_eq!(got["task"]["creator"], lead.id);
    assert_eq!(got["task"]["agent_id"], worker.id);
    assert_eq!(got["task"]["state"], "assigned");
    assert_eq!(got["task"]["cost"], Value::Null);
    assert_eq!(got["usage"]["turns"], 1);
    assert_eq!(got["budget_status"]["crossed"], json!(["turns"]));
    assert_eq!(got["budget_status"]["notification"], message);
    rig.finish_turn(&worker.pane);
    rig.finish_turn(&lead.pane);
    rig.test.rig.close();
}

// Break caught: unavailable usage is made up as an exceeded zero budget.
#[test]
fn unreported_usage_cannot_cross_zero_cost_or_turn_limits() {
    let mut rig = start_without_reminders();
    let task = rig
        .call(
            "task.create",
            json!({
                "title":"Unknown usage", "measure":{"kind":"owner"},
                "budget":{"cost":{"microdollars":0},"turns":0}
            }),
            None,
        )
        .unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut watch = rig.watch();
    let sentinel = create_due(&rig);
    watch.wait("budget pass with unavailable usage", |event| {
        budget_note(event, &sentinel)
    });
    let got = rig.call("task.get", json!({"task_id":task}), None).unwrap();
    assert_eq!(got["budget_status"]["crossed"], json!([]));
    assert_eq!(got["budget_status"]["notification"], Value::Null);
    assert!(!rig
        .test
        .tail(0)
        .iter()
        .any(|event| budget_note(event, &task)));
    rig.test.rig.close();
}

// Break caught: public create bypasses the pure validator, or an optional
// budget status changes the established response for tasks without limits.
#[test]
fn budget_create_validation_and_unbudgeted_get_contract() {
    let mut rig = start_without_reminders();
    let refused = rig
        .call(
            "task.create",
            json!({
                "title":"Invalid budget", "measure":{"kind":"owner"},
                "budget":{"cost":{"microdollars":-1}}
            }),
            None,
        )
        .unwrap_err();
    assert_eq!(refused.0, "invalid_params");
    assert!(rig.call("task.list", json!({}), None).unwrap()["tasks"]
        .as_array()
        .unwrap()
        .is_empty());
    let task = rig
        .call(
            "task.create",
            json!({"title":"No budget", "measure":{"kind":"owner"}}),
            None,
        )
        .unwrap();
    let got = rig
        .call("task.get", json!({"task_id":task["task_id"]}), None)
        .unwrap();
    assert!(got.get("budget_status").is_none());
    rig.test.rig.close();
}
