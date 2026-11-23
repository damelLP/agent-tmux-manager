//! Default-off routing through the real daemon and its unchanged delivery path.
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

use serde_json::json;
use stand_in::StandInMode;
use std::collections::BTreeMap;
use support::{reference, Confirmation, MessageRig};

// Break: default-off drops untagged posts, or a broadcast shares a wake/receipt
// across recipients instead of using the existing per-seat delivery path.
#[test]
fn untagged_lead_broadcast_delivers_distinct_wakes_on_the_default_config() {
    let rig = MessageRig::start(Confirmation::Generous);
    let info = rig.call("daemon.info", json!({}), None).unwrap();
    for method in ["router.attach", "channel.route"] {
        assert!(
            info["methods"].as_array().unwrap().contains(&json!(method)),
            "{method} not registered"
        );
    }
    let lead = rig.ready_agent("lead", StandInMode::Claude);
    let first = rig.ready_agent("first", StandInMode::Pi);
    let second = rig.ready_agent("second", StandInMode::Claude);
    let seats = [&first, &second];
    let cursors = seats.map(|s| rig.test.rig.event_cursor(&s.pane));
    let mut watch = rig.watch();
    let body = "a note for the project";
    let (project, _) = lead.id.split_once('/').unwrap();
    let post = rig
        .call(
            "channel.post",
            json!({"channel":project,"body":body}),
            Some(&lead.pane),
        )
        .unwrap();
    let id = &post["post_id"];
    let route = watch.wait("stopgap answers lead post", |e| {
        e["kind"] == "channel.routed" && e["body"]["post"] == *id
    });
    assert_eq!(
        route["body"]["by"],
        json!({"kind":"stopgap","cause":"disabled"})
    );
    assert_eq!(route["body"]["wake"], json!([first.id, second.id]));

    let mut rows = BTreeMap::<String, String>::new();
    let mut receipts = BTreeMap::new();
    while receipts.len() < seats.len() {
        let event = watch.wait("a broadcast row or its receipt", |e| {
            match e["kind"].as_str() {
                Some("msg.sent") => e["body"]["post"] == *id,
                Some("msg.delivered") => rows
                    .values()
                    .any(|w| e["body"]["message_id"].as_str() == Some(w)),
                _ => false,
            }
        });
        if event["kind"] == "msg.sent" {
            let row = &event["body"];
            assert_eq!(row["from"], lead.id);
            assert_eq!(row["body"], body);
            let to = row["to"].as_str().unwrap().to_owned();
            let wake = format!("m{}", event["seq"].as_u64().unwrap());
            assert_ne!(&json!(wake), id);
            assert!(rows.insert(to, wake).is_none());
        } else {
            let body = &event["body"];
            assert_eq!(body["confirmation"], "prompt_submit_exact_reference");
            assert!(receipts
                .insert(
                    body["message_id"].as_str().unwrap().to_owned(),
                    body["session_id"].clone()
                )
                .is_none());
        }
    }
    assert_eq!(rows.len(), seats.len());
    assert_ne!(rows[&first.id], rows[&second.id]);
    for (seat, cursor) in seats.into_iter().zip(cursors) {
        let wake = &rows[&seat.id];
        rig.test.rig.wait_event(&seat.pane, cursor, |e| {
            e["kind"] == "accepted" && e["text"] == reference(wake)
        });
        assert_eq!(receipts[wake].as_str(), seat.session.as_deref());
    }
}
