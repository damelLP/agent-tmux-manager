//! The start line on a private daemon, end to end: once `agent.up` and
//! `agent.clear` end Ready, the daemon types `[mN] New session: run atm me`
//! into the agent's stand-in through the message runtime, and the stand-in's
//! own prompt event confirms it. The stand-ins are model-free and never
//! execute what they are given. Waits are on committed events (`Watch`),
//! stand-in events after a cursor, and RPC answers.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[allow(dead_code)] // this file uses a subset of the lifecycle rig
#[path = "support/agent_lifecycle.rs"]
mod lifecycle;
#[path = "support/stand_in.rs"]
mod stand_in;
#[allow(dead_code)] // and of the message rig
#[path = "support/message_delivery.rs"]
mod support;

use lifecycle::Started;
use serde_json::{json, Value};
use stand_in::{StandInMode, StandInPane};
use support::{about, reference, Confirmation, MessageRig, Watch};

/// The text typed for start line `id`.
fn line(id: &str) -> String {
    format!("[{id}] New session: run atm me")
}

/// A message's id is its `msg.sent` event's sequence number.
fn id_of(sent: &Value) -> String {
    format!("m{}", sent["seq"])
}

fn seq(event: &Value) -> u64 {
    event["seq"].as_u64().expect("event seq")
}

/// The committed start line for `agent`: the canonical note from system.
fn start_line_for(agent: &str) -> impl Fn(&Value) -> bool {
    let body = json!({
        "from": "system",
        "to": agent,
        "kind": "note",
        "body": "New session: run atm me",
        "attachments": [],
        "task": null,
    });
    move |event| event["kind"] == "msg.sent" && event["body"] == body
}

/// The stand-in's event that it accepted exactly this prompt.
fn accepted(text: &str) -> impl Fn(&Value) -> bool {
    let text = text.to_owned();
    move |event| event["kind"] == "accepted" && event["text"] == text.as_str()
}

/// A message rig whose daemon starts agents with the `mode` stand-in and has
/// start lines on (the stand-in rig turns them off by default).
fn start_line_rig(mode: StandInMode) -> MessageRig {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let launcher = rig.test.rig.launcher(mode);
    rig.test.rig.set_daemon_env("ATM_SPAWN_BIN", launcher);
    rig.test.rig.set_daemon_env("ATM_SPAWN_START_LINE", "on");
    rig.test.rig.restart_daemon();
    assert_eq!(rig.test.rpc("agent.list", json!({}), None), json!([]));
    rig.assert_delivery_config_in_force();
    rig
}

/// `agent.up` of `lead`; its start line is typed, confirmed and its turn
/// finished. Returns the agent's id, its pane, its session and the line's id.
fn up_through_start_line(
    rig: &MessageRig,
    watch: &mut Watch,
    mode: StandInMode,
) -> (String, StandInPane, String, String) {
    let started = Started::up(&rig.test, "lead", &lifecycle::started_config(mode));
    assert_eq!(started.result["state"], "ready", "{}", started.result);
    let agent = started.id.clone();
    let session = rig.test.agent(&agent)["session_id"]
        .as_str()
        .expect("a session bound at the start")
        .to_owned();
    // Queued once the agent is Ready, never before.
    let bound = watch.wait("the start's binding", |e| {
        e["kind"] == "agent.session_bound"
    });
    let sent = watch.wait("the start line", start_line_for(&agent));
    assert!(seq(&bound) < seq(&sent));
    let id = id_of(&sent);
    // Typed as its own text, and confirmed by the session's prompt event.
    rig.test
        .rig
        .wait_event(&started.pane, 0, accepted(&line(&id)));
    let delivered = watch.wait("the start line's receipt", about("msg.delivered", &id));
    assert_eq!(delivered["body"]["session_id"], session.as_str());
    assert_eq!(
        delivered["body"]["confirmation"],
        "prompt_submit_exact_reference"
    );
    rig.finish_turn(&started.pane);
    (agent, started.pane, session, id)
}

/// Start `agent.clear` of `agent` and return once the stand-in holds the
/// clear in its gap; `join` the thread after moving the stand-in's clock.
fn hold_clear(
    rig: &MessageRig,
    agent: &str,
    pane: &StandInPane,
) -> std::thread::JoinHandle<Result<Value, (String, String)>> {
    let cursor = rig.test.rig.event_cursor(pane);
    let socket = rig.test.rig.socket();
    let params = json!({"agent": agent, "timeout_ms": 120_000});
    let clear = std::thread::spawn(move || support::call_on(&socket, "agent.clear", params, None));
    rig.test.rig.wait_event(pane, cursor, |e| {
        e["kind"] == "rendered" && e["state"] == "clearing"
    });
    clear
}

/// No record of the stand-in's has the clear command and a start line in one
/// text: they were never joined into one prompt.
fn never_joined(rig: &MessageRig, pane: &StandInPane) {
    for record in rig.test.rig.journal(pane) {
        let text = record["text"].as_str().unwrap_or_default();
        assert!(
            !((text.contains("/new") || text.contains("/clear")) && text.contains("New session")),
            "the clear command and a start line were joined: {record}"
        );
    }
}

/// After `agent.up` the start line is the agent's first prompt; after
/// `agent.clear` a second one, with a new id, is the new session's first.
/// Exactly these two prompts, one per operation.
fn start_line_after_up_and_after_clear(mode: StandInMode) {
    let mut rig = start_line_rig(mode);
    let mut watch = rig.watch();
    let (agent, pane, session, first) = up_through_start_line(&rig, &mut watch, mode);

    let cursor = rig.test.rig.event_cursor(&pane);
    let clear = hold_clear(&rig, &agent, &pane);
    rig.advance(&pane, 100, "idle");
    let cleared = clear.join().unwrap().expect("agent.clear");
    let new = cleared["session_id"].as_str().expect("a new session");
    assert_ne!(new, session);
    let bound = watch.wait("the clear's binding", |e| {
        e["kind"] == "agent.session_bound" && e["body"]["session_id"] == new
    });
    let sent = watch.wait("the clear's start line", start_line_for(&agent));
    assert!(seq(&bound) < seq(&sent));
    let second = id_of(&sent);
    assert_ne!(second, first);
    rig.test
        .rig
        .wait_event(&pane, cursor, accepted(&line(&second)));
    let delivered = watch.wait("its receipt", about("msg.delivered", &second));
    assert_eq!(delivered["body"]["session_id"], new);
    rig.finish_turn(&pane);

    assert_eq!(rig.accepted_since(&pane, 0), [line(&first), line(&second)]);
    never_joined(&rig, &pane);
    watch.catch_up(&rig);
    assert_eq!(watch.all(start_line_for(&agent)).len(), 2);
    rig.test.rig.close();
}

#[test]
fn start_line_after_up_and_after_clear_pi() {
    start_line_after_up_and_after_clear(StandInMode::Pi);
}

#[test]
fn start_line_after_up_and_after_clear_claude() {
    start_line_after_up_and_after_clear(StandInMode::Claude);
}

/// Messages to one agent are typed oldest first: one sent while the clear is
/// held is typed before the clear's start line, which follows when that
/// message's turn ends.
#[test]
fn a_message_sent_during_a_clear_is_typed_before_the_start_line() {
    let mode = StandInMode::Pi;
    let mut rig = start_line_rig(mode);
    let mut watch = rig.watch();
    let (agent, pane, _, _) = up_through_start_line(&rig, &mut watch, mode);

    let cursor = rig.test.rig.event_cursor(&pane);
    let clear = hold_clear(&rig, &agent, &pane);
    let message = rig.send(None, &agent, "for the new session");
    rig.advance(&pane, 100, "idle");
    clear.join().unwrap().expect("agent.clear");
    let sent = watch.wait("the clear's start line", start_line_for(&agent));
    let line_id = id_of(&sent);

    rig.test
        .rig
        .wait_event(&pane, cursor, accepted(&reference(&message)));
    watch.wait("the message's receipt", about("msg.delivered", &message));
    // The start line waits for the message's turn.
    assert_eq!(rig.accepted_since(&pane, cursor), [reference(&message)]);
    rig.finish_turn(&pane);
    rig.test
        .rig
        .wait_event(&pane, cursor, accepted(&line(&line_id)));
    watch.wait("the start line's receipt", about("msg.delivered", &line_id));
    rig.finish_turn(&pane);
    assert_eq!(
        rig.accepted_since(&pane, cursor),
        [reference(&message), line(&line_id)]
    );
    never_joined(&rig, &pane);
    rig.test.rig.close();
}
