//! Task effects use the existing private real-daemon/message stand-in rig.
//! No submitted message text is executed and no model CLI runs.
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
use stand_in::StandInMode;
use support::{about, reference, Confirmation, MessageRig};

// Break caught: a task notification bypasses the guarded reference/inbox path,
// loses task/file attribution, or reports to manager/owner instead of creator.
#[test]
fn assigned_brief_and_submitted_report_travel_through_guarded_pi_references() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", StandInMode::Pi);
    let worker = rig.ready_agent("worker", StandInMode::Pi);
    let mut watch = rig.watch();
    let worker_cursor = rig.test.rig.event_cursor(&worker.pane);
    let lead_cursor = rig.test.rig.event_cursor(&lead.pane);
    let created=rig.call("task.create",json!({"title":"Tasks é🙂","measure":{"kind":"mechanical","command":"check"},"files":[{"path":"brief.md","kind":"brief"}]}),Some(&lead.pane)).unwrap();
    let id = created["task_id"].as_str().unwrap();
    let assigned = rig
        .call(
            "task.assign",
            json!({"task_id":id,"agent_id":worker.id}),
            Some(&lead.pane),
        )
        .unwrap();
    let brief = assigned["notification"]["message_id"].as_str().unwrap();
    watch.wait("brief durable enqueue", |e| {
        e["kind"] == "msg.sent" && e["body"]["task"] == id && e["body"]["kind"] == "brief"
    });
    watch.wait(
        "worker accepted brief reference",
        about("msg.delivered", brief),
    );
    rig.test.rig.wait_event(&worker.pane, worker_cursor, |e| {
        e["kind"] == "accepted" && e["text"] == reference(brief)
    });
    assert_eq!(
        rig.accepted_since(&worker.pane, worker_cursor),
        [reference(brief)]
    );
    let inbox = rig.inbox_entry(Some(&worker.pane), None, brief);
    assert_eq!(
        inbox["content"],
        json!({"from":lead.id,"to":worker.id,"kind":"brief","body":format!("Task {id} assigned: Tasks é🙂\nInspect the task: atm task get {id}\nRead the attached brief files before starting."),"attachments":["brief.md"],"task":id})
    );
    assert_eq!(inbox["read_at"], Value::Null);
    let context = rig.atm_json(Some(&worker.pane), &["me", "--format", "json"]);
    assert_eq!(context["agent_id"], worker.id);
    rig.call(
        "task.set_state",
        json!({"task_id":id,"state":"working"}),
        Some(&worker.pane),
    )
    .unwrap();
    let submitted = rig
        .call(
            "task.submit",
            json!({"task_id":id,"files":[{"path":"report.md","kind":"report"}]}),
            Some(&worker.pane),
        )
        .unwrap();
    let report = submitted["notification"]["message_id"].as_str().unwrap();
    watch.wait(
        "creator accepted report reference",
        about("msg.delivered", report),
    );
    rig.test.rig.wait_event(&lead.pane, lead_cursor, |e| {
        e["kind"] == "accepted" && e["text"] == reference(report)
    });
    assert_eq!(
        rig.accepted_since(&lead.pane, lead_cursor),
        [reference(report)]
    );
    let inbox = rig.inbox_entry(Some(&lead.pane), None, report);
    assert_eq!(
        inbox["content"],
        json!({"from":worker.id,"to":lead.id,"kind":"report","body":format!("Task {id} submitted (result): Tasks é🙂\nSubmitted by {}; inspect the task: atm task get {id}\nRead the attached submission files before giving a verdict.",worker.id),"attachments":["report.md"],"task":id})
    );
    assert_eq!(inbox["read_at"], Value::Null);
    rig.call(
        "task.verdict",
        json!({"task_id":id,"verdict":"accepted"}),
        Some(&lead.pane),
    )
    .unwrap();
    assert_eq!(
        rig.call("task.get", json!({"task_id":id}), Some(&worker.pane))
            .unwrap()["task"]["state"],
        "accepted"
    );
    watch.catch_up(&rig);
    assert_eq!(
        watch
            .all(|e| e["kind"] == "msg.sent" && e["body"]["task"] == id)
            .len(),
        2
    );
    for pane in [&worker.pane, &lead.pane] {
        assert!(!rig
            .test
            .rig
            .journal(pane)
            .iter()
            .any(|e| e["kind"] == "accepted"
                && e["text"].as_str().is_some_and(|s| s.contains("Tasks é🙂"))));
        rig.finish_turn(pane);
    }
    rig.test.rig.close();
}

// Break caught: assignment types into an existing draft or rolls back because delivery is held.
#[test]
fn draft_holds_automatic_brief_without_reverting_task_or_typing_into_editor() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let worker = rig.ready_agent("worker", StandInMode::Pi);
    let cursor = rig.test.rig.event_cursor(&worker.pane);
    rig.test.rig.key(&worker.pane, "x");
    rig.test.rig.wait_event(&worker.pane, cursor, |e| {
        e["kind"] == "rendered" && e["draft"] == "x"
    });
    let screen = rig.test.rig.capture_styled(&worker.pane);
    let created = rig
        .call(
            "task.create",
            json!({"title":"Held task","measure":{"kind":"owner"}}),
            None,
        )
        .unwrap();
    let id = created["task_id"].as_str().unwrap();
    let mut watch = rig.watch();
    let assigned = rig
        .call(
            "task.assign",
            json!({"task_id":id,"agent_id":worker.id}),
            None,
        )
        .unwrap();
    let message = assigned["notification"]["message_id"].as_str().unwrap();
    watch.wait("automatic brief enqueued", |e| {
        e["kind"] == "msg.sent" && e["body"]["task"] == id
    });
    // Runtime progress has no subscription/audit event for a recoverable draft.
    // Use the existing rig's bounded condition poll, not a delay as evidence.
    rig.test.rig.wait("message runtime observed the draft", || {
        let entry = rig.inbox_entry(Some(&worker.pane), None, message);
        entry["delivery"]["phase"] == "waiting"
            && entry["delivery"]["reason"] == "input contains a draft"
    });
    assert_eq!(
        rig.call("task.get", json!({"task_id":id}), None).unwrap()["task"]["state"],
        "assigned"
    );
    assert!(rig.accepted_since(&worker.pane, cursor).is_empty());
    assert_eq!(rig.test.rig.capture_styled(&worker.pane), screen);
    watch.catch_up(&rig);
    assert!(watch
        .all(about("msg.delivery_attempted", message))
        .is_empty());
    let cleared = rig.test.rig.event_cursor(&worker.pane);
    rig.test.rig.key(&worker.pane, "C-u");
    rig.test.rig.wait_event(&worker.pane, cleared, |e| {
        e["kind"] == "rendered" && e["draft"] == ""
    });
    watch.wait(
        "brief delivered after draft cleared",
        about("msg.delivered", message),
    );
    rig.test.rig.wait_event(&worker.pane, cleared, |e| {
        e["kind"] == "accepted" && e["text"] == reference(message)
    });
    assert_eq!(
        rig.accepted_since(&worker.pane, cursor),
        [reference(message)]
    );
    rig.finish_turn(&worker.pane);
    rig.test.rig.close();
}
