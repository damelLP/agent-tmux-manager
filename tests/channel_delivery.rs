//! Channel posts end to end on a private daemon (unit U5): `channel.post` by
//! raw RPC, the unchanged delivery path, and the model-free stand-ins adopted
//! as seats. A wake is an ordinary message row: each seat's pane gets its own
//! `[mW] Read: atm inbox --id mW` line and its own receipt.
//!
//! Every wait is on a committed record (`watch`) or a stand-in's own event;
//! none is on time.
//!
//! Claude and pi stand-ins only: the Codex stand-in is adopted Starting, never
//! Ready (message_delivery.rs, `codex`, ignored for that reason). The real
//! Codex CLI receives a wake in the unit's sealed real-harness check.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[allow(dead_code)] // this file uses a subset of the lifecycle rig
#[path = "support/agent_lifecycle.rs"]
mod lifecycle;
#[path = "support/stand_in.rs"]
mod stand_in;
#[allow(dead_code)] // this file uses a subset of the message rig
#[path = "support/message_delivery.rs"]
mod support;

use serde_json::{json, Value};
use stand_in::StandInMode;
use support::{about, reference, Confirmation, MessageRig, Recipient};

fn accepted(text: &str) -> impl Fn(&Value) -> bool {
    let text = text.to_owned();
    move |event| event["kind"] == "accepted" && event["text"] == text.as_str()
}

fn project(seat: &Recipient) -> String {
    seat.id.split('/').next().unwrap().to_owned()
}

fn post(rig: &MessageRig, from: Option<&Recipient>, params: Value) -> Value {
    rig.call("channel.post", params.clone(), from.map(|r| &r.pane))
        .unwrap_or_else(|error| panic!("channel.post {params}: {error:?}\n{}", rig.daemon_log()))
}

/// The wake row the post result names for this seat.
fn wake_of(result: &Value, seat: &Recipient) -> String {
    result["woke"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["to"] == seat.id.as_str())
        .unwrap_or_else(|| panic!("{} not woken: {result}", seat.id))["message_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Each seat's pane accepts its own reference line, and each receipt confirms
/// its own row with the session the seat was adopted with. Receipts commit in
/// any order, so they are collected as a set in commit order.
fn delivered(rig: &MessageRig, watch: &mut support::Watch, wakes: &[(&Recipient, String, u64)]) {
    for (seat, wake, cursor) in wakes {
        rig.test
            .rig
            .wait_event(&seat.pane, *cursor, accepted(&reference(wake)));
    }
    let ids: Vec<&str> = wakes.iter().map(|(_, wake, _)| wake.as_str()).collect();
    let mut receipts = std::collections::BTreeMap::new();
    while receipts.len() < ids.len() {
        let receipt = watch.wait("the wakes' receipts", |e| {
            e["kind"] == "msg.delivered"
                && e["body"]["message_id"]
                    .as_str()
                    .is_some_and(|id| ids.contains(&id))
        });
        let id = receipt["body"]["message_id"].as_str().unwrap().to_owned();
        assert!(receipts.insert(id, receipt).is_none(), "a second receipt");
    }
    for (seat, wake, _) in wakes {
        let receipt = &receipts[wake];
        assert_eq!(
            receipt["body"]["confirmation"],
            "prompt_submit_exact_reference"
        );
        assert_eq!(
            receipt["body"]["session_id"].as_str(),
            seat.session.as_deref(),
            "{}",
            seat.id
        );
    }
}

// Break: one row for several seats, a shared or post-id reference line, a
// receipt that confirms another seat's row, or an ack by post id that reads
// nothing or another seat's row.
#[test]
fn post_tagging_two_seats_pastes_each_its_own_reference_and_acks_by_post_id() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", StandInMode::Claude);
    let worker = rig.ready_agent("worker", StandInMode::Pi);
    let cursors = [
        rig.test.rig.event_cursor(&lead.pane),
        rig.test.rig.event_cursor(&worker.pane),
    ];
    let mut watch = rig.watch();
    let result = post(
        &rig,
        None,
        json!({"channel":project(&lead),"body":"hold W2","tags":["lead","worker"]}),
    );
    let post_id = result["post_id"].as_str().unwrap().to_owned();
    let wakes = [wake_of(&result, &lead), wake_of(&result, &worker)];
    assert_ne!(wakes[0], wakes[1]);
    assert!(!wakes.contains(&post_id));
    delivered(
        &rig,
        &mut watch,
        &[
            (&lead, wakes[0].clone(), cursors[0]),
            (&worker, wakes[1].clone(), cursors[1]),
        ],
    );
    for ((seat, wake), cursor) in [&lead, &worker].into_iter().zip(&wakes).zip(cursors) {
        assert_eq!(rig.accepted_since(&seat.pane, cursor), [reference(wake)]);
        let entry = rig.inbox_entry(Some(&seat.pane), None, wake);
        assert_eq!(entry["content"]["post"], post_id.as_str());
        assert_eq!(entry["post_context"]["channel"], project(&lead));
    }

    // Each seat acks by the post's id and reads its own row.
    for (seat, wake) in [&lead, &worker].into_iter().zip(&wakes) {
        let ack = rig.atm_json(
            Some(&seat.pane),
            &["msg", "ack", post_id.as_str(), "--format", "json"],
        );
        assert_eq!(ack["count"], 1, "{}", seat.id);
        let read = watch.wait("the ack by post id", |e| {
            e["kind"] == "msg.read" && e["body"]["message_ids"] == json!([wake])
        });
        assert_eq!(read["body"]["message_ids"], json!([wake]));
    }
    rig.test.rig.close();
}

// Break: a reply by wake id placed in a new thread, or a thread view that
// cannot find it.
#[test]
fn reply_by_wake_id_lands_in_the_thread() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", StandInMode::Claude);
    let cursor = rig.test.rig.event_cursor(&lead.pane);
    let mut watch = rig.watch();
    let asked = post(&rig, None, json!({"to":lead.id,"body":"is W1 on track?"}));
    let wake = wake_of(&asked, &lead);
    delivered(&rig, &mut watch, &[(&lead, wake.clone(), cursor)]);

    let reply = post(&rig, Some(&lead), json!({"reply_to":wake,"body":"yes"}));
    assert_eq!(reply["thread"], asked["post_id"]);
    assert_eq!(reply["channel"], asked["channel"]);
    let thread = rig
        .call(
            "channel.history",
            json!({"channel":asked["channel"],"thread":asked["post_id"]}),
            None,
        )
        .unwrap();
    let ids: Vec<&Value> = thread["posts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["id"])
        .collect();
    assert_eq!(ids, [&asked["post_id"], &reply["post_id"]]);
    rig.test.rig.close();
}

// V5: a ten-seat post stays inside the scheduler's bounds. Break: a wake
// lost, delivered twice, or confirmed for the wrong seat.
#[test]
fn post_with_ten_tags_reaches_ten_seats() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", StandInMode::Claude);
    let modes = [StandInMode::Claude, StandInMode::Pi];
    let mut seats = vec![lead];
    for i in 1..10 {
        seats.push(rig.ready_agent(&format!("seat-{i}"), modes[i % 2]));
    }
    let cursors: Vec<u64> = seats
        .iter()
        .map(|s| rig.test.rig.event_cursor(&s.pane))
        .collect();
    let mut watch = rig.watch();
    let tags: Vec<String> = seats
        .iter()
        .map(|s| s.id.split('/').nth(1).unwrap().to_owned())
        .collect();
    let result = post(
        &rig,
        None,
        json!({"channel":project(&seats[0]),"body":"all hands","tags":tags}),
    );
    assert_eq!(result["woke"].as_array().unwrap().len(), 10);
    let wakes: Vec<(&Recipient, String, u64)> = seats
        .iter()
        .zip(&cursors)
        .map(|(seat, cursor)| (seat, wake_of(&result, seat), *cursor))
        .collect();
    delivered(&rig, &mut watch, &wakes);
    for (seat, wake, cursor) in &wakes {
        assert_eq!(rig.accepted_since(&seat.pane, *cursor), [reference(wake)]);
    }
    watch.catch_up(&rig);
    for seat in &seats {
        let wake = wake_of(&result, seat);
        assert_eq!(watch.all(about("msg.delivered", &wake)).len(), 1);
        assert!(watch.all(about("msg.delivery_failed", &wake)).is_empty());
    }
    rig.test.rig.close();
}

// Break: msg.send's path changed by channels, or its message missing from
// the direct channel's history.
#[test]
fn msg_send_round_trip_is_unchanged_and_shows_in_channel_history() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", StandInMode::Claude);
    let cursor = rig.test.rig.event_cursor(&lead.pane);
    let mut watch = rig.watch();
    let id = rig.send(None, &lead.id, "plain message");
    let sent = watch.wait("the message", |e| {
        e["kind"] == "msg.sent" && format!("m{}", e["seq"]) == id
    });
    assert!(sent["body"].get("post").is_none(), "{sent}");
    delivered(&rig, &mut watch, &[(&lead, id.clone(), cursor)]);
    let ack = rig.atm_json(Some(&lead.pane), &["msg", "ack", &id, "--format", "json"]);
    assert_eq!(ack["count"], 1);

    let channel = format!("{}+owner", lead.id);
    let history = rig
        .call("channel.history", json!({"channel":channel}), None)
        .unwrap();
    let posts = history["posts"].as_array().unwrap();
    assert_eq!(posts.len(), 1, "{history}");
    assert_eq!(posts[0]["id"], id.as_str());
    assert_eq!(posts[0]["legacy"], true);
    assert_eq!(posts[0]["author"], "owner");
    assert_eq!(posts[0]["body"], "plain message");
    rig.test.rig.close();
}
