//! Task/message facts across two real private daemon lifetimes.
//! All waits are RPC/process/event fences; no model CLI or live socket.
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
use atm_org::{
    Address, DecisionCommand, DecisionId, DecisionRequest, DecisionStatus, OrgCommand, OrgState,
    ReplayStats,
};
use atmd::eventlog::{read_from, EventLog, Stream};
use serde_json::{json, Value};
use stand_in::{StandInMode, StandInPane};
use std::{path::Path, time::Duration};
use support::{Confirmation, MessageRig};

fn replay(path: &Path) -> OrgState {
    let mut state = OrgState::default();
    let mut stats = ReplayStats::default();
    for e in read_from(path, Stream::Events, 0) {
        let e = e.unwrap();
        stats
            .apply_parts(&mut state, e.seq, e.ts, &e.kind, &e.body)
            .unwrap();
    }
    state
}
// D2's RPC is not part of this base. Seed one decision only while atmd is
// stopped, using the actual D1 decide/event codecs, not an imagined wire body.
fn seed_pending_decision(rig: &mut MessageRig, requester: &str) -> DecisionId {
    rig.test.rig.stop_daemon();
    assert!(rig.test.rig.daemon_pids().is_empty());
    let path = rig.test.rig.root().join("state");
    let state = replay(&path);
    let now = chrono::Utc::now();
    let request:DecisionRequest=serde_json::from_value(json!({"requester":requester,"kind":"policy","title":"Pending owner answer","context":"Restart fixture","files":[],"options":["wait","continue"],"recommended":"wait","may_be_undone":true,"changes_intent":false,"deadline":null})).unwrap();
    let events = state
        .decide(
            &OrgCommand::Decision(DecisionCommand::Request(request)),
            now,
        )
        .unwrap();
    assert_eq!(events.len(), 1);
    let event = events.first().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let log = EventLog::open(&path, Stream::Events).unwrap();
    let mut closed = log.subscribe();
    let seq = runtime
        .block_on(log.append(now, event.kind(), event.to_body()))
        .unwrap();
    drop(log);
    // Sender closure is the writer's exit fence: its stream lock is released
    // before this next daemon is allowed to open the same state directory.
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(60), async {
            while closed.recv().await.is_ok() {}
        })
        .await
        .unwrap();
    });
    let id = DecisionId::new(seq);
    assert_eq!(
        replay(&path).decision(&id).unwrap().status,
        DecisionStatus::Pending
    );
    id
}
fn call(rig: &MessageRig, pane: Option<&StandInPane>, method: &str, params: Value) -> Value {
    rig.call(method, params, pane).unwrap()
}
fn create(rig: &MessageRig, pane: Option<&StandInPane>, params: Value) -> String {
    call(rig, pane, "task.create", params)["task_id"]
        .as_str()
        .unwrap()
        .into()
}
fn assign(rig: &MessageRig, id: &str, agent: &str) -> Value {
    call(
        rig,
        None,
        "task.assign",
        json!({"task_id":id,"agent_id":agent}),
    )
}
fn inbox_facts(rig: &MessageRig, pane: Option<&StandInPane>) -> Value {
    let inbox = call(rig, pane, "msg.inbox", json!({}));
    json!({"unread_count":inbox["unread_count"],"messages":inbox["messages"].as_array().unwrap().iter().map(|m|json!({"message_id":m["message_id"],"content":m["content"],"status":m["status"],"sent_at":m["sent_at"],"read_at":m["read_at"]})).collect::<Vec<_>>()})
}

// Break caught: tasks/attachments/budget/review effects are transient, or
// replay sends fresh briefs/reports and loses message read state.
#[test]
fn real_daemon_restart_preserves_tasks_reviews_messages_and_pending_decision() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", StandInMode::Pi);
    let worker = rig.ready_agent("worker", StandInMode::Pi);
    // This provisional dependency is deliberately distinct from the pending
    // decision seeded at the restart below; T1 stores it without existence checks.
    let dependency = DecisionId::new(99);
    rig.hold_turn(&worker.pane);
    rig.hold_turn(&lead.pane);
    let working = create(
        &rig,
        Some(&lead.pane),
        json!({"title":"Working","measure":{"kind":"mechanical","command":"check"},"budget":{"cost":{"microdollars":1000},"turns":40,"deadline":"2030-01-01T00:00:00Z"},"rests_on":dependency,"files":[{"path":"brief.md","kind":"brief","note":"read first"}]}),
    );
    assign(&rig, &working, &worker.id);
    call(
        &rig,
        Some(&worker.pane),
        "task.set_state",
        json!({"task_id":working,"state":"working"}),
    );
    call(
        &rig,
        Some(&worker.pane),
        "task.attach",
        json!({"task_id":working,"file":{"path":"evidence.log","kind":"evidence"}}),
    );
    let waiting = create(
        &rig,
        None,
        json!({"title":"Waiting","measure":{"kind":"owner"}}),
    );
    assign(&rig, &waiting, &worker.id);
    call(
        &rig,
        Some(&worker.pane),
        "task.set_state",
        json!({"task_id":waiting,"state":"waiting","reason":"owner answer needed"}),
    );
    let judged = create(
        &rig,
        Some(&lead.pane),
        json!({"title":"Reviewed","measure":{"kind":"reviewed","rubric":"rubric.md"}}),
    );
    assign(&rig, &judged, &worker.id);
    call(
        &rig,
        Some(&worker.pane),
        "task.submit",
        json!({"task_id":judged,"files":[{"path":"report.md","kind":"report"}]}),
    );
    let review = create(
        &rig,
        None,
        json!({"title":"Review","kind":"review","judges":judged,"measure":{"kind":"owner"}}),
    );
    assign(&rig, &review, &lead.id);
    let submitted = call(
        &rig,
        Some(&lead.pane),
        "task.submit",
        json!({"task_id":review,"files":[{"path":"review.md","kind":"report"}]}),
    );
    call(
        &rig,
        None,
        "msg.ack",
        json!({"ids":[submitted["notification"]["message_id"]]}),
    );
    call(
        &rig,
        None,
        "task.review",
        json!({"task_id":judged,"review_task":review}),
    );
    call(
        &rig,
        Some(&lead.pane),
        "task.verdict",
        json!({"task_id":judged,"verdict":"accepted","review_task":review,"note":"review passed"}),
    );
    let member_created = create(
        &rig,
        Some(&worker.pane),
        json!({"title":"Member-created","parent":working,"measure":{"kind":"owner"}}),
    );
    let ids = [
        working.clone(),
        waiting.clone(),
        judged.clone(),
        review.clone(),
        member_created.clone(),
    ];
    let before: Vec<_> = ids
        .iter()
        .map(|id| call(&rig, None, "task.get", json!({"task_id":id})))
        .collect();
    assert_eq!(before[0]["task"]["state"], "working");
    assert_eq!(before[0]["task"]["rests_on"], json!(dependency));
    assert_eq!(before[1]["task"]["state"], "waiting");
    for result in &before[2..4] {
        assert_eq!(result["task"]["state"], "accepted");
    }
    assert_eq!(before[4]["task"]["creator"], worker.id);
    let list = call(&rig, None, "task.list", json!({}));
    assert_eq!(list["tasks"].as_array().unwrap().len(), 5);
    let context = rig.atm_json(Some(&worker.pane), &["me", "--format", "json"]);
    assert_eq!(
        context["open_tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["task_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [working.as_str(), waiting.as_str()]
    );
    let inboxes = [
        inbox_facts(&rig, None),
        inbox_facts(&rig, Some(&lead.pane)),
        inbox_facts(&rig, Some(&worker.pane)),
    ];
    let messages = rig
        .test
        .tail(0)
        .into_iter()
        .filter(|e| {
            e["kind"] == "msg.sent"
                && e["body"]["task"]
                    .as_str()
                    .is_some_and(|id| ids.iter().any(|s| s == id))
        })
        .count();
    assert_eq!(messages, 6);
    let end = rig.test.cursor();
    let decision = seed_pending_decision(&mut rig, &worker.id);
    rig.test.rig.start_daemon();
    for (id, want) in ids.iter().zip(before) {
        assert_eq!(call(&rig, None, "task.get", json!({"task_id":id})), want);
    }
    assert_eq!(call(&rig, None, "task.list", json!({})), list);
    let after = rig.atm_json(Some(&worker.pane), &["me", "--format", "json"]);
    assert_eq!(after["open_tasks"], context["open_tasks"]);
    assert_eq!(after["agent_id"], worker.id);
    assert_eq!(
        [
            inbox_facts(&rig, None),
            inbox_facts(&rig, Some(&lead.pane)),
            inbox_facts(&rig, Some(&worker.pane))
        ],
        inboxes
    );
    assert!(!rig
        .test
        .tail(end)
        .iter()
        .any(|e| e["kind"] == "task.created"
            || e["kind"] == "msg.sent"
                && e["body"]["task"]
                    .as_str()
                    .is_some_and(|id| ids.iter().any(|s| s == id))));
    rig.test.rig.stop_daemon();
    let state = replay(&rig.test.rig.root().join("state"));
    assert_eq!(
        state.decision(&decision).unwrap().status,
        DecisionStatus::Pending
    );
    assert_eq!(
        state.decision(&decision).unwrap().request.requester,
        worker.id.parse::<Address>().unwrap()
    );
    rig.test.rig.close();
}
