//! The start line on a private daemon, end to end: once `agent.up` and
//! `agent.clear` end Ready, the daemon types
//! `[mN] New session: run the shell command atm me`
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
use support::{about, notice, reference, Confirmation, MessageRig, Watch};

/// The text typed for start line `id`.
fn line(id: &str) -> String {
    format!("[{id}] New session: run the shell command atm me")
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
        "body": "New session: run the shell command atm me",
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
    start_line_rig_with(mode, |_| {})
}

/// As [`start_line_rig`], with `prepare` run before the daemon's restart,
/// which reads the config.
fn start_line_rig_with(mode: StandInMode, prepare: impl FnOnce(&MessageRig)) -> MessageRig {
    let mut rig = MessageRig::start(Confirmation::Generous);
    prepare(&rig);
    let launcher = rig.test.rig.launcher(mode);
    rig.test.rig.set_daemon_env("ATM_SPAWN_BIN", launcher);
    rig.test.rig.set_daemon_env("ATM_SPAWN_START_LINE", "on");
    rig.test.rig.restart_daemon();
    assert_eq!(rig.test.rpc("agent.list", json!({}), None), json!([]));
    rig.assert_delivery_config_in_force();
    rig
}

/// Set the daemon config's stable-idle window to `window`, read at the
/// daemon's next start; returns the window it replaced.
fn set_stable_idle(rig: &MessageRig, window: &str) -> String {
    let path = rig.test.rig.root().join("config/atm/config.toml");
    let config = std::fs::read_to_string(&path).unwrap();
    let mut replaced = None;
    let mut lines = Vec::new();
    for line in config.lines() {
        match line.strip_prefix("stable_idle = ") {
            Some(old) => {
                assert!(replaced.replace(old.to_owned()).is_none(), "{config}");
                lines.push(format!("stable_idle = {window}"));
            }
            None => lines.push(line.to_owned()),
        }
    }
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    replaced.unwrap_or_else(|| panic!("no stable_idle in {config}"))
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
    let sent = watch.wait("the clear's start line", start_line_for(&agent));
    let bound = watch.wait("the clear's binding", |e| {
        e["kind"] == "agent.session_bound" && e["body"]["session_id"] == new
    });
    assert!(seq(&sent) < seq(&bound));
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

/// Queue the ordinary message in the old session's running turn. Hold its
/// Stop until /clear has started: clear can read the idle frame, but delivery
/// still sees the old Working row and cannot paste into the old session.
#[test]
fn a_clear_start_line_precedes_an_ordinary_message_queued_before_clear() {
    let mode = StandInMode::Claude;
    let mut rig = start_line_rig(mode);
    let mut watch = rig.watch();
    let (agent, pane, old, _) = up_through_start_line(&rig, &mut watch, mode);

    rig.hold_turn(&pane);
    let message = rig.send(None, &agent, "for the new session");
    assert_eq!(
        rig.inbox_entry(None, Some(&agent), &message)["status"],
        "queued"
    );
    let held = rig
        .test
        .rig
        .control(&pane, json!({"command": "hold_next_hook", "event": "Stop"}));
    assert!(held["error"].is_null(), "{held}");
    rig.finish_turn(&pane);
    let row = rig
        .test
        .rig
        .wait_for_row(&pane, |row| row.id.as_str() == old);
    assert_eq!(row.status, atm_core::SessionStatus::Working);
    let cursor = rig.test.rig.event_cursor(&pane);
    let clear = hold_clear(&rig, &agent, &pane);
    let released = rig
        .test
        .rig
        .control(&pane, json!({"command": "release_hook"}));
    assert!(released["error"].is_null(), "{released}");
    // Hold the first prompt's exact receipt so a message sent after Ready
    // must still wait for the start line, regardless of observation timing.
    let held = rig.test.rig.control(
        &pane,
        json!({"command": "hold_next_hook", "event": "UserPromptSubmit"}),
    );
    assert!(held["error"].is_null(), "{held}");
    rig.advance(&pane, 100, "idle");
    let cleared = clear.join().unwrap().expect("agent.clear");
    let new = cleared["session_id"].as_str().expect("a new session");
    assert_ne!(new, old);
    let line_id = id_of(&watch.wait("the clear's start line", start_line_for(&agent)));
    let first = rig
        .test
        .rig
        .wait_event(&pane, cursor, |e| e["kind"] == "accepted");
    assert_eq!(
        first["text"],
        line(&line_id),
        "first input to the new session: {first}"
    );
    let newer = rig.send(None, &agent, "after the clear returned Ready");
    for id in [&message, &newer] {
        let entry = rig.inbox_entry(None, Some(&agent), id);
        assert_eq!(entry["status"], "queued", "{entry}");
        assert_eq!(entry["delivery"]["blocked_by"], line_id, "{entry}");
    }
    assert_eq!(rig.accepted_since(&pane, cursor), [line(&line_id)]);
    let released = rig
        .test
        .rig
        .control(&pane, json!({"command": "release_hook"}));
    assert!(released["error"].is_null(), "{released}");
    let start_receipt = watch.wait("the start line's receipt", about("msg.delivered", &line_id));
    assert_eq!(start_receipt["body"]["session_id"], new);
    rig.finish_turn(&pane);
    let ordinary_receipt = watch.wait("the ordinary receipt", about("msg.delivered", &message));
    assert_eq!(ordinary_receipt["body"]["session_id"], new);
    assert!(seq(&start_receipt) < seq(&ordinary_receipt));
    rig.finish_turn(&pane);
    let newer_receipt = watch.wait("the newer receipt", about("msg.delivered", &newer));
    assert_eq!(newer_receipt["body"]["session_id"], new);
    assert!(seq(&ordinary_receipt) < seq(&newer_receipt));
    assert_eq!(
        rig.accepted_since(&pane, cursor),
        [line(&line_id), reference(&message), reference(&newer)]
    );
    never_joined(&rig, &pane);
    rig.test.rig.close();
}

/// Restart after a clear's Ready and before any attempt. As in the launch
/// replay case, an idle stabilization window longer than the test's hang
/// guard is never awaited; a held stand-in turn fences the actual restart.
#[test]
fn a_replayed_clear_start_line_still_precedes_older_messages() {
    let mode = StandInMode::Claude;
    let mut short = String::new();
    let mut rig = start_line_rig_with(mode, |rig| {
        short = set_stable_idle(rig, "{ secs = 600, nanos = 0 }");
    });
    // Adopt rather than launch so the long idle window never needs to let
    // a first start line through.
    let recipient = rig.ready_agent("lead", mode);
    let (agent, pane) = (recipient.id, recipient.pane);
    let mut watch = rig.watch();
    let clear = hold_clear(&rig, &agent, &pane);
    let older = rig.send(None, &agent, "still waiting after restart");
    rig.advance(&pane, 100, "idle");
    let result = clear.join().unwrap().expect("agent.clear");
    let new = result["session_id"].as_str().unwrap();
    let start = id_of(&watch.wait("the clear's start line", start_line_for(&agent)));
    rig.hold_turn(&pane);
    watch.catch_up(&rig);
    for id in [&start, &older] {
        assert!(watch.all(about("msg.delivery_attempted", id)).is_empty());
    }
    let end = rig.test.cursor();
    drop(watch);

    set_stable_idle(&rig, &short);
    let cursor = rig.test.rig.event_cursor(&pane);
    rig.test.rig.restart_daemon();
    rig.assert_delivery_config_in_force();
    let mut watch = Watch::open(&rig, end);
    rig.finish_turn(&pane);
    let first = rig
        .test
        .rig
        .wait_event(&pane, cursor, |e| e["kind"] == "accepted");
    assert_eq!(first["text"], line(&start));
    let start_receipt = watch.wait(
        "start delivered after replay",
        about("msg.delivered", &start),
    );
    assert_eq!(start_receipt["body"]["session_id"], new);
    assert_eq!(
        start_receipt["body"]["confirmation"],
        "prompt_submit_exact_reference"
    );
    rig.finish_turn(&pane);
    let older_receipt = watch.wait(
        "ordinary message delivered after replay",
        about("msg.delivered", &older),
    );
    assert_eq!(older_receipt["body"]["session_id"], new);
    assert_eq!(
        older_receipt["body"]["confirmation"],
        "prompt_submit_exact_reference"
    );
    assert_eq!(
        rig.accepted_since(&pane, cursor),
        [line(&start), reference(&older)]
    );
    watch.catch_up(&rig);
    assert_eq!(watch.all(about("msg.delivery_attempted", &start)).len(), 1);
    rig.test.rig.close();
}

/// A start line still queued when the daemon restarts is typed once by the
/// new daemon, as its own text, and confirmed by the session's prompt event:
/// the startup replay expects the line's text, not an inbox reference. No
/// hold, and nothing about it reaches the owner.
///
/// The first daemon's stable-idle window is one no test outlasts, so the line
/// is still unattempted when the test starts a turn of its own; from then on
/// the busy session keeps it queued, across the restart, until that turn ends
/// under the new daemon's short window. The window is never awaited. Claude
/// only: the pi stand-in loses its transport across a restart (a known
/// limit), so its delivery never starts.
#[test]
fn a_start_line_queued_across_a_restart_is_typed_once_and_confirmed() {
    let mode = StandInMode::Claude;
    let mut short = String::new();
    let mut rig = start_line_rig_with(mode, |rig| {
        short = set_stable_idle(rig, "{ secs = 600, nanos = 0 }");
    });
    let mut watch = rig.watch();
    let started = Started::up(&rig.test, "lead", &lifecycle::started_config(mode));
    assert_eq!(started.result["state"], "ready", "{}", started.result);
    let agent = started.id.clone();
    let pane = started.pane;
    let session = rig.test.agent(&agent)["session_id"]
        .as_str()
        .expect("a session bound at the start")
        .to_owned();
    let id = id_of(&watch.wait("the start line", start_line_for(&agent)));
    rig.hold_turn(&pane);
    watch.catch_up(&rig);
    assert!(
        watch.all(about("msg.delivery_attempted", &id)).is_empty(),
        "attempted before the restart: {:?}",
        watch.seen
    );
    let end = rig.test.cursor();
    drop(watch);

    set_stable_idle(&rig, &short);
    let cursor = rig.test.rig.event_cursor(&pane);
    rig.test.rig.restart_daemon();
    rig.assert_delivery_config_in_force();
    let mut watch = Watch::open(&rig, end);
    rig.finish_turn(&pane);

    // The fence: the line's receipt, or the hold the owner is told of instead.
    let delivered = about("msg.delivered", &id);
    let held = notice(&format!("{id} blocked: "));
    let outcome = watch.wait("the line's receipt or its hold", |e| {
        delivered(e) || held(e)
    });
    assert_eq!(outcome["kind"], "msg.delivered", "held instead: {outcome}");
    assert_eq!(outcome["body"]["session_id"], session.as_str());
    assert_eq!(
        outcome["body"]["confirmation"],
        "prompt_submit_exact_reference"
    );
    rig.finish_turn(&pane);

    // Typed once, as its own text, by the new daemon.
    assert_eq!(rig.accepted_since(&pane, cursor), [line(&id)]);
    let log = rig.test.tail(0);
    let attempts: Vec<_> = log
        .iter()
        .filter(|e| about("msg.delivery_attempted", &id)(e))
        .collect();
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    assert!(seq(attempts[0]) > end, "{attempts:?}");
    // No hold and no duplicate risk for the owner to settle.
    let told: Vec<_> = log
        .iter()
        .filter(|e| {
            e["kind"] == "msg.sent"
                && e["body"]["to"] == "owner"
                && e["body"]["body"].as_str().is_some_and(|body| {
                    body.starts_with(&format!("{id} ")) || body.contains("accept-duplicate-risk")
                })
        })
        .collect();
    assert!(told.is_empty(), "the owner was told: {told:?}");
    let entry = rig.inbox_entry(None, Some(&agent), &id);
    assert_eq!(entry["status"], "delivered", "{entry}");
    rig.test.rig.close();
}
