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

fn assert_restart_task(before: &Value, mut after: Value) {
    let state = before["task"]["state"].as_str().unwrap();
    if !matches!(state, "accepted" | "cancelled") {
        // Only the running state's duration includes time at the read. All
        // historical durations and state keys must still compare exactly.
        let seconds = &before["usage"]["seconds_in_state"][state];
        let later = after["usage"]["seconds_in_state"].get_mut(state).unwrap();
        assert!(
            later.as_u64().unwrap() >= seconds.as_u64().unwrap(),
            "elapsed {state} seconds went backwards: {seconds} -> {later}"
        );
        *later = seconds.clone();

        // A live deadline can cross between reads. Cost/turn crossings and the
        // durable notification id cannot change; neither can terminal budgets.
        if let Some(crossed) = after.pointer_mut("/budget_status/crossed") {
            let earlier = before["budget_status"]["crossed"].as_array().unwrap();
            let crossed = crossed.as_array_mut().unwrap();
            let deadline = json!("deadline");
            assert!(
                !earlier.contains(&deadline) || crossed.contains(&deadline),
                "deadline crossing went backwards"
            );
            if !earlier.contains(&deadline) && crossed.contains(&deadline) {
                assert_eq!(crossed.pop(), Some(deadline));
            }
        }
    }
    // Retain the full response comparison, including persisted Task.cost,
    // unknown/new fields and every usage value that is not clock-derived.
    assert_eq!(after, *before);
}

// Synthetic: fixed event times exercise the real usage/budget projections,
// without waiting for a second boundary or starting another daemon.
fn task_snapshot_at(now: &str) -> Value {
    let mut state = OrgState::default();
    let mut stats = ReplayStats::default();
    for (seq, ts, kind, body) in [
        (
            1,
            "2026-01-01T00:00:00Z",
            "task.created",
            json!({"title":"Work","kind":"work","creator":"owner","measure":{"kind":"owner"},"budget":{"cost":{"microdollars":1000},"turns":40,"deadline":"2026-01-01T00:00:05Z"},"files":[{"path":"brief.md","kind":"brief"}]}),
        ),
        (
            2,
            "2026-01-01T00:00:02Z",
            "task.state_changed",
            json!({"task_id":"t1","state":"working","reason":null,"by":"owner"}),
        ),
    ] {
        stats
            .apply_parts(&mut state, seq, ts.parse().unwrap(), kind, &body)
            .unwrap();
    }
    let task = state.task(&"t1".parse().unwrap()).unwrap();
    let now = now.parse().unwrap();
    let usage = state.task_cost(&task.id, now).unwrap();
    serde_json::to_value(atmd::org::tasks::GetResult {
        task: task.clone(),
        budget_status: atm_org::budget::budget_status(&state, task, &usage, now),
        usage,
    })
    .unwrap()
}

// Break caught: an exact task.get comparison rejects a later read of unchanged
// persisted facts merely because a running state's elapsed seconds advanced.
#[test]
fn restart_comparison_allows_a_later_read_without_changing_durable_facts() {
    let before = task_snapshot_at("2026-01-01T00:00:03Z");
    let after = task_snapshot_at("2026-01-01T00:00:04Z");
    assert_eq!(
        before["usage"]["seconds_in_state"],
        json!({"open":2,"working":1})
    );
    assert_eq!(
        after["usage"]["seconds_in_state"],
        json!({"open":2,"working":2})
    );
    assert_ne!(
        after, before,
        "the old exact comparison must reject this read"
    );
    assert_restart_task(&before, after);
}

// Break caught: a read-time deadline crossing is treated as lost persisted
// budget data, even though the actual budget and notification are unchanged.
#[test]
fn restart_comparison_allows_a_live_deadline_to_cross_only_forward() {
    let before = task_snapshot_at("2026-01-01T00:00:04Z");
    let after = task_snapshot_at("2026-01-01T00:00:05Z");
    assert_eq!(before["budget_status"]["crossed"], json!([]));
    assert_eq!(after["budget_status"]["crossed"], json!(["deadline"]));
    assert_restart_task(&before, after.clone());
    let mut backwards = after.clone();
    backwards["budget_status"]["crossed"] = json!([]);
    assert!(std::panic::catch_unwind(|| assert_restart_task(&after, backwards)).is_err());
}

// Break caught: stripping usage or a whole response subtree hides replay loss.
#[test]
fn restart_comparison_rejects_every_non_clock_change() {
    let before = task_snapshot_at("2026-01-01T00:00:03Z");
    assert_restart_task(&before, before.clone());
    for (path, value) in [
        ("/task/files", json!([])),
        ("/task/budget", Value::Null),
        ("/task/changed_at", json!("2026-01-01T00:00:03Z")),
        ("/task/cost", before["usage"].clone()),
        ("/usage/turns", json!(1)),
        ("/usage/review_rounds", json!(1)),
        ("/usage/changes_requested", json!(1)),
        ("/usage/cost", json!({"microdollars":1})),
        ("/usage/peak_context_percent", json!(1)),
        ("/usage/seconds_in_state/open", json!(3)),
        ("/usage/seconds_in_state/working", json!(0)),
        ("/usage/seconds_in_state", json!({"open":2})),
        ("/usage/seconds_in_state", json!({"working":1})),
        (
            "/usage/seconds_in_state",
            json!({"open":2,"working":1,"waiting":0}),
        ),
        ("/budget_status/crossed", json!(["cost"])),
        ("/budget_status/crossed", json!(["turns"])),
        ("/budget_status/crossed", json!(["deadline", "deadline"])),
        ("/budget_status/notification", json!("m3")),
    ] {
        let mut after = before.clone();
        let field = after.pointer_mut(path).unwrap();
        assert_ne!(*field, value, "ineffective mutation: {path}");
        *field = value;
        assert!(
            std::panic::catch_unwind(|| assert_restart_task(&before, after)).is_err(),
            "restart comparison hid a change to {path}"
        );
    }
    let mut after = before.clone();
    after["unexpected_field"] = json!(true);
    assert!(std::panic::catch_unwind(|| assert_restart_task(&before, after)).is_err());
}

// Synthetic: mark the projection terminal to exercise the comparison's frozen
// branch. No duration or deadline is read-time-derived after a terminal state.
#[test]
fn restart_comparison_keeps_terminal_usage_and_budgets_exact() {
    for state in ["accepted", "cancelled"] {
        let mut before = task_snapshot_at("2026-01-01T00:00:03Z");
        before["task"]["state"] = json!(state);
        if state == "accepted" {
            before["task"]["cost"] = before["usage"].clone();
        }
        assert_restart_task(&before, before.clone());
        for (path, value) in [
            ("/usage/seconds_in_state/working", json!(2)),
            ("/budget_status/crossed", json!(["deadline"])),
        ] {
            let mut after = before.clone();
            *after.pointer_mut(path).unwrap() = value;
            assert!(
                std::panic::catch_unwind(|| assert_restart_task(&before, after)).is_err(),
                "{state} comparison hid a change to {path}"
            );
        }
    }
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
    let task_comms = |e: &Value| {
        matches!(e["kind"].as_str(), Some("msg.sent" | "channel.posted"))
            && e["body"]["task"]
                .as_str()
                .is_some_and(|id| ids.iter().any(|s| s == id))
    };
    let comms: Vec<_> = rig.test.tail(0).into_iter().filter(task_comms).collect();
    let posts: Vec<_> = comms
        .iter()
        .filter(|e| e["kind"] == "channel.posted")
        .collect();
    assert_eq!(posts.len(), 6, "each task wake has a native post");
    assert_eq!(comms.len(), 12);
    for posted in posts {
        let seq = posted["seq"].as_u64().unwrap();
        let post = format!("m{seq}");
        let rows: Vec<_> = comms
            .iter()
            .filter(|e| e["kind"] == "msg.sent" && e["body"]["post"] == post)
            .collect();
        assert_eq!(rows.len(), 1);
        assert!(rows[0]["seq"].as_u64().unwrap() > seq);
        assert_eq!(posted["body"]["wake"], json!([rows[0]["body"]["to"]]));
    }
    let end = rig.test.cursor();
    let decision = seed_pending_decision(&mut rig, &worker.id);
    rig.test.rig.start_daemon();
    for (id, want) in ids.iter().zip(before) {
        assert_restart_task(&want, call(&rig, None, "task.get", json!({"task_id":id})));
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
        .any(|e| e["kind"] == "task.created" || task_comms(e)));
    assert_eq!(
        rig.test
            .tail(0)
            .into_iter()
            .filter(task_comms)
            .collect::<Vec<_>>(),
        comms
    );
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
