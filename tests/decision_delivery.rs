//! Real private daemon/tmux with shared model-free harnesses; never paid CLIs.
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
use support::{about, notice, reference, Confirmation, FocusClient, MessageRig, Recipient, Watch};

fn raise(rig: &MessageRig, seat: &Recipient) -> String {
    let result = rig.call("decision.request", json!({"kind":"plan","title":"Candidate-only plan","context":"Private question","options":["yes","no"],"recommended":"yes","may_be_undone":true,"changes_intent":true}), Some(&seat.pane)).unwrap();
    result["decision_id"].as_str().unwrap().to_owned()
}
fn resolve(rig: &MessageRig, id: &str) -> Value {
    rig.atm_json(
        None,
        &[
            "decide",
            id,
            "yes",
            "owner answer",
            "--rule",
            "--format",
            "json",
        ],
    )
}
fn sent(watch: &mut Watch, seat: &Recipient, decision: &str) -> String {
    let record = watch.wait("the resolution answer", |e| {
        e["kind"] == "msg.sent" && e["body"]["to"] == seat.id && e["body"]["kind"] == "answer"
    });
    assert_eq!(record["body"]["from"], "system");
    assert_eq!(
        record["body"]["body"],
        format!("Decision {decision} resolved: yes\nNote: owner answer\nStanding rule: yes")
    );
    format!("m{}", record["seq"])
}
fn accepted(rig: &MessageRig, seat: &Recipient, cursor: u64, id: &str) {
    let text = reference(id);
    rig.test.rig.wait_event(&seat.pane, cursor, |e| {
        e["kind"] == "accepted" && e["text"] == text
    });
    assert_eq!(rig.accepted_since(&seat.pane, cursor), [text]);
    assert!(!rig
        .test
        .rig
        .journal(&seat.pane)
        .iter()
        .any(|e| e.to_string().contains("owner answer")));
}

#[test]
fn decision_answer_reference_and_state_survive_real_daemon_restart() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let seat = rig.ready_agent("worker", StandInMode::Pi);
    let mut watch = rig.watch();
    let cursor = rig.test.rig.event_cursor(&seat.pane);
    let decision = raise(&rig, &seat);
    let pending = rig.atm_json(None, &["decisions", "--format", "json"]);
    assert_eq!(pending["pending_count"], 1);
    assert_eq!(pending["decisions"][0]["id"], decision);
    assert!(String::from_utf8(rig.atm(None, &["status"]).stdout)
        .unwrap()
        .contains("1 decisions"));
    let answer = resolve(&rig, &decision);
    assert_eq!(answer["changed"], true);
    let id = sent(&mut watch, &seat, &decision);
    let delivered = watch.wait("the confirmed answer", about("msg.delivered", &id));
    assert_eq!(
        delivered["body"]["confirmation"],
        "prompt_submit_exact_reference"
    );
    accepted(&rig, &seat, cursor, &id);
    let inbox = rig.inbox_entry(Some(&seat.pane), None, &id);
    assert_eq!(
        inbox["content"]["body"],
        format!("Decision {decision} resolved: yes\nNote: owner answer\nStanding rule: yes")
    );
    assert_eq!(inbox["read_at"], Value::Null);
    assert_eq!(
        rig.atm_json(Some(&seat.pane), &["msg", "ack", &id, "--format", "json"])["count"],
        1
    );
    rig.finish_turn(&seat.pane);
    rig.test.rig.restart_daemon();
    assert_eq!(
        rig.call("decision.get", json!({"id":decision}), None)
            .unwrap()["decision"],
        answer["decision"]
    );
    assert_eq!(resolve(&rig, &decision)["changed"], false);
    let inbox = rig.call("msg.inbox", json!({"for":seat.id}), None).unwrap();
    assert_eq!(
        inbox["messages"].as_array().unwrap().len(),
        1,
        "restart/repeat must not enqueue another answer"
    );
    assert_eq!(inbox["messages"][0]["message_id"], id);
    assert!(!inbox["messages"][0]["read_at"].is_null());
    assert_eq!(
        rig.call("decision.list", json!({"count_only":true}), None)
            .unwrap()["pending_count"],
        0
    );
    rig.test.rig.close();
}

#[test]
fn decision_answer_obeys_the_existing_owner_focus_guard() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let seat = rig.ready_agent("worker", StandInMode::Claude);
    let mut client = FocusClient::attach(&rig, &seat.pane);
    client.focus(&rig, true);
    let mut watch = rig.watch();
    let cursor = rig.test.rig.event_cursor(&seat.pane);
    let screen = rig.test.rig.capture_styled(&seat.pane);
    let decision = raise(&rig, &seat);
    resolve(&rig, &decision);
    let id = sent(&mut watch, &seat, &decision);
    watch.wait(
        "the focus guard",
        notice(&format!(
            "{id} waiting: owner is focused on {}",
            seat.pane.pane_id
        )),
    );
    rig.assert_untouched(&seat.pane, cursor, &screen);
    let held = rig.inbox_entry(None, Some(&seat.id), &id);
    assert_eq!(held["delivery"]["attempt"], 0);
    client.focus(&rig, false);
    watch.wait("the released answer receipt", about("msg.delivered", &id));
    accepted(&rig, &seat, cursor, &id);
    rig.finish_turn(&seat.pane);
    drop(client);
    rig.test.rig.close();
}

#[test]
fn decision_answer_does_not_overwrite_an_occupied_editor() {
    use std::io::Write;
    let mut rig = MessageRig::start(Confirmation::Generous);
    // Fixture-only deadline: wait for the existing runtime's committed hold,
    // never sleep to infer that no input happened or assert elapsed time.
    let config = rig.test.rig.root().join("config/atm/config.toml");
    std::fs::OpenOptions::new()
        .append(true)
        .open(config)
        .unwrap()
        .write_all(b"readiness_timeout = { secs = 1, nanos = 0 }\n")
        .unwrap();
    rig.test.rig.restart_daemon();
    let seat = rig.ready_agent("worker", StandInMode::Pi);
    let before_draft = rig.test.rig.event_cursor(&seat.pane);
    rig.test.rig.key(&seat.pane, "unfinished draft");
    rig.test.rig.wait_event(&seat.pane, before_draft, |e| {
        e["kind"] == "rendered" && e["draft"] == "unfinished draft"
    });
    let cursor = rig.test.rig.event_cursor(&seat.pane);
    let screen = rig.test.rig.capture_styled(&seat.pane);
    let mut watch = rig.watch();
    let decision = raise(&rig, &seat);
    resolve(&rig, &decision);
    let id = sent(&mut watch, &seat, &decision);
    let hold = watch.wait(
        "the existing draft hold",
        notice(&format!("{id} blocked: ")),
    );
    assert!(hold["body"]["body"]
        .as_str()
        .unwrap()
        .contains("input draft persisted"));
    rig.assert_untouched(&seat.pane, cursor, &screen);
    assert!(rig.accepted_since(&seat.pane, cursor).is_empty());
    let before_clear = rig.test.rig.event_cursor(&seat.pane);
    rig.test.rig.key(&seat.pane, "C-u");
    rig.test.rig.wait_event(&seat.pane, before_clear, |e| {
        e["kind"] == "rendered" && e["draft"] == ""
    });
    // Draft holds are recoverable, unlike uncertain-input/restart latches.
    watch.wait(
        "the automatically resumed answer receipt",
        about("msg.delivered", &id),
    );
    accepted(&rig, &seat, cursor, &id);
    rig.finish_turn(&seat.pane);
    rig.test.rig.close();
}
