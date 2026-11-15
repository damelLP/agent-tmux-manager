//! End-to-end message delivery on a private daemon: the real `atm msg` and
//! `atm inbox`, committed events through `watch`, and the shared model-free
//! stand-ins adopted as agents. No model CLI runs and no submitted text is
//! executed.
//!
//! Delivered means the harness accepted the one-line inbox reference. It is
//! never proof that the body was fetched, read or acted on.
//!
//! Each ignored test names the exact thing it waits for. None has an empty
//! body: where a stand-in control is missing the test runs as far as it can
//! and then fails with the reason, so `--ignored` cannot pass it vacuously.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[allow(dead_code)] // this file uses a subset of the lifecycle rig
#[path = "support/agent_lifecycle.rs"]
mod lifecycle;
#[path = "support/stand_in.rs"]
mod stand_in;
#[path = "support/message_delivery.rs"]
mod support;

use serde_json::{json, Value};
use stand_in::StandInMode;
use support::{about, notice, reference, Confirmation, FocusClient, MessageRig, Watch};

/// Blank lines, both quote kinds, a tab, leading spaces and non-ASCII text.
const BODY: &str = "first line\n\n  \"double\" and 'single' quotes\n\tταχύ ✓ 日本語\nlast line";

/// A message's id is its `msg.sent` event's sequence number.
fn id_of(sent: &Value) -> String {
    format!("m{}", sent["seq"])
}

fn seq(event: &Value) -> u64 {
    event["seq"].as_u64().expect("event seq")
}

/// The stand-in's event that it accepted exactly this prompt.
fn accepted(text: &str) -> impl Fn(&Value) -> bool {
    let text = text.to_owned();
    move |event| event["kind"] == "accepted" && event["text"] == text.as_str()
}

/// The stand-in's frame, acknowledged by the terminal, that shows this draft.
fn drafted(text: &str) -> impl Fn(&Value) -> bool {
    let text = text.to_owned();
    move |event| event["kind"] == "rendered" && event["draft"] == text.as_str()
}

/// Everything committed for these messages' delivery after the watch opened,
/// read up to the log's end.
fn attempts(watch: &mut Watch, rig: &MessageRig, id: &str) -> Vec<Value> {
    watch.catch_up(rig);
    watch.all(about("msg.delivery_attempted", id))
}

/// The rig itself, live on every base: the CLI, the sender's attribution, the
/// exact body, the explicit acknowledgement, the `[delivery]` config and the
/// committed events through `watch`. It claims nothing about pasting.
#[test]
fn rig_send_inbox_ack_round_trip() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", StandInMode::Claude);
    let mut watch = rig.watch();

    // The owner writes to the agent; the agent reads its own inbox.
    let id = rig.send(None, &lead.id, BODY);
    let sent = watch.wait("the message to the agent", |e| {
        e["kind"] == "msg.sent" && e["body"]["to"] == lead.id.as_str()
    });
    assert_eq!(id_of(&sent), id);
    assert_eq!(sent["body"]["from"], "owner");
    assert_eq!(sent["body"]["body"], BODY);
    let entry = rig.inbox_entry(Some(&lead.pane), None, &id);
    assert_eq!(entry["content"]["body"], BODY);
    assert_eq!(entry["content"]["from"], "owner");
    assert_eq!(entry["read_at"], json!(null), "fetching is not reading");
    // The owner can also fetch the agent's message with --for.
    assert_eq!(
        rig.inbox_entry(None, Some(&lead.id), &id)["content"]["body"],
        BODY
    );

    // The agent answers from its pane: the sender is the agent bound to it.
    let answer = rig.send(Some(&lead.pane), "owner", "done\n\nsecond paragraph");
    let sent = watch.wait("the answer to the owner", |e| {
        e["kind"] == "msg.sent" && e["body"]["to"] == "owner"
    });
    assert_eq!(id_of(&sent), answer);
    assert_eq!(sent["body"]["from"], lead.id.as_str());
    let entry = rig.inbox_entry(None, None, &answer);
    assert_eq!(entry["content"]["body"], "done\n\nsecond paragraph");
    assert_eq!(entry["status"], "delivered");
    assert_eq!(entry["read_at"], json!(null));

    // Reading is the owner's explicit act, once.
    let ack = ["msg", "ack", answer.as_str(), "--format", "json"];
    assert_eq!(rig.atm_json(None, &ack)["count"], 1);
    let read = watch.wait("the acknowledgement", |e| e["kind"] == "msg.read");
    assert_eq!(read["body"]["message_ids"], json!([answer]));
    assert_eq!(rig.atm_json(None, &ack)["count"], 0);
    assert_ne!(rig.inbox_entry(None, None, &answer)["read_at"], json!(null));
    rig.test.rig.close();
}

/// A message to a Ready agent is delivered as its one-line reference, and
/// the body is read from the inbox, byte for byte.
mod delivered_reference_and_exact_inbox_body {
    use super::*;

    fn run(mode: StandInMode) {
        let mut rig = MessageRig::start(Confirmation::Generous);
        let lead = rig.ready_agent("lead", mode);
        let mut watch = rig.watch();
        let cursor = rig.test.rig.event_cursor(&lead.pane);

        let id = rig.send(None, &lead.id, BODY);
        let attempted = watch.wait("the attempt", about("msg.delivery_attempted", &id));
        assert_eq!(attempted["body"]["attempt"], 1, "{mode:?}");
        assert_eq!(attempted["body"]["pane"], lead.pane.pane_id.as_str());
        let delivered = watch.wait("the receipt", about("msg.delivered", &id));
        assert_eq!(
            delivered["body"]["session_id"].as_str(),
            lead.session.as_deref(),
            "{mode:?}: the receipt names the session that was adopted"
        );

        // The harness accepted the reference, once, and nothing else.
        rig.test
            .rig
            .wait_event(&lead.pane, cursor, accepted(&reference(&id)));
        assert_eq!(rig.accepted_since(&lead.pane, cursor), [reference(&id)]);
        for record in rig.test.rig.journal(&lead.pane) {
            assert!(
                !record.to_string().contains("ταχύ"),
                "the body reached the terminal: {record}"
            );
        }
        assert_eq!(attempts(&mut watch, &rig, &id).len(), 1);
        assert!(watch.all(about("msg.delivery_failed", &id)).is_empty());

        // The recipient fetches the exact body with the command it was given.
        let entry = rig.inbox_entry(Some(&lead.pane), None, &id);
        assert_eq!(entry["content"]["body"], BODY, "{mode:?}");
        assert_eq!(entry["status"], "delivered");
        assert_eq!(entry["delivery"]["attempt"], 1);
        assert_eq!(
            entry["delivery"]["session_id"].as_str(),
            lead.session.as_deref()
        );
        // Delivery is not reading: the acknowledgement is the agent's own act.
        assert_eq!(entry["read_at"], json!(null));
        assert!(watch.all(|e| e["kind"] == "msg.read").is_empty());
        let ack = ["msg", "ack", id.as_str(), "--format", "json"];
        assert_eq!(rig.atm_json(Some(&lead.pane), &ack)["count"], 1);
        let read = watch.wait("the acknowledgement", |e| e["kind"] == "msg.read");
        assert_eq!(read["body"]["message_ids"], json!([id]));
        assert_eq!(rig.atm_json(Some(&lead.pane), &ack)["count"], 0);

        // The reference started a turn; the stand-in never executes it.
        rig.finish_turn(&lead.pane);
        rig.test.rig.close();
    }

    #[test]
    fn claude() {
        run(StandInMode::Claude);
    }

    #[test]
    fn pi() {
        run(StandInMode::Pi);
    }

    #[test]
    #[ignore = "the Codex stand-in at 200x50 is adopted Starting, never Ready: adoption finds no safe editor on its screen, after a finished turn too"]
    fn codex() {
        run(StandInMode::Codex);
    }
}

/// An attempt whose paste is proven not to have been sent is failed and
/// retried on the policy's delays, with increasing attempt numbers.
///
/// The proof stays the scripted `proven_no_effect_retries_1_2_4_then_final`
/// family in atmd's delivery tests until the stand-in can refuse a paste.
#[test]
#[ignore = "missing stand-in control: refuse the next paste before it is sent (a proven no-effect between the attempt's commit and its paste guard)"]
fn safe_retry_after_known_zero_effect() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", StandInMode::Claude);
    let mut watch = rig.watch();
    let id = rig.send(None, &lead.id, "retried after a proven no-effect");
    watch.wait("the message", |e| e["kind"] == "msg.sent");
    // From here the test needs the first guarded paste to be refused with
    // nothing sent: then msg.delivery_failed (not final), attempt 2 after the
    // first retry delay, one accepted reference, msg.delivered.
    rig.test.rig.close();
    panic!(
        "{id}: the stand-in has no control that refuses a paste before it is sent; \
         neither tmux nor the fixture can place a guard refusal between the \
         attempt's commit and its paste"
    );
}

/// The prompt hook is lost after Enter: the message is held for the owner,
/// never failed and never typed again. Ending the turn below is incidental;
/// this does not distinguish a hold with activity from one without it.
#[test]
fn lost_hook_blocks() {
    let mut rig = MessageRig::start(Confirmation::Default);
    let lead = rig.ready_agent("lead", StandInMode::Claude);
    rig.stand_in_control(&lead.pane, "/atm/drop-next UserPromptSubmit", "drop armed");
    let mut watch = rig.watch();
    let cursor = rig.test.rig.event_cursor(&lead.pane);

    let id = rig.send(None, &lead.id, "the hook for this one is lost");
    watch.wait("the attempt", about("msg.delivery_attempted", &id));
    // Enter landed and the stand-in dropped the one hook that would confirm.
    let events = &rig.test.rig;
    events.wait_event(&lead.pane, cursor, accepted(&reference(&id)));
    events.wait_event(&lead.pane, cursor, |e| {
        e["kind"] == "dropped" && e["event"] == "UserPromptSubmit"
    });
    // Activity of the attempted session after the attempt: its turn ends.
    rig.finish_turn(&lead.pane);

    // The fence: the daemon decided, and told the owner.
    let told = watch.wait("the blocked notice", notice(&format!("{id} blocked: ")));
    let text = told["body"]["body"].as_str().unwrap();
    assert!(text.contains("waiting behind it: none. "), "{text}");
    assert!(
        text.contains(&format!("atm msg retry {id} --accept-duplicate-risk")),
        "a latched hold offers the owner's retry: {text}"
    );
    assert_eq!(attempts(&mut watch, &rig, &id).len(), 1);
    assert!(
        watch.all(about("msg.delivery_failed", &id)).is_empty(),
        "an applied paste is never failed"
    );
    assert!(watch.all(about("msg.delivered", &id)).is_empty());
    assert_eq!(rig.accepted_since(&lead.pane, cursor), [reference(&id)]);
    let entry = rig.inbox_entry(None, Some(&lead.id), &id);
    assert_eq!(entry["status"], "delivering", "{entry}");
    assert_eq!(entry["delivery"]["phase"], "blocked", "{entry}");
    assert_eq!(entry["delivery"]["attempt"], 1, "{entry}");
    rig.test.rig.close();
}

/// Nothing is typed into a pane the owner is looking at. The same message is
/// pasted once the owner looks away, so the silence was the focus guard.
#[test]
fn focused_pane_never_pasted() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", StandInMode::Claude);
    let mut client = FocusClient::attach(&rig, &lead.pane);
    client.focus(&rig, true);
    let mut watch = rig.watch();
    let cursor = rig.test.rig.event_cursor(&lead.pane);
    let screen = rig.test.rig.capture_styled(&lead.pane);

    let id = rig.send(None, &lead.id, "not while the owner is typing");
    // The fence: the daemon evaluated the message against this pane.
    let pane = &lead.pane.pane_id;
    watch.wait(
        "the focus notice",
        notice(&format!("{id} waiting: owner is focused on {pane}")),
    );
    assert!(attempts(&mut watch, &rig, &id).is_empty());
    rig.assert_untouched(&lead.pane, cursor, &screen);
    let entry = rig.inbox_entry(None, Some(&lead.id), &id);
    assert_eq!(entry["status"], "queued", "{entry}");
    assert_eq!(entry["delivery"]["attempt"], 0, "{entry}");

    client.focus(&rig, false);
    let attempted = watch.wait("the attempt", about("msg.delivery_attempted", &id));
    assert_eq!(attempted["body"]["attempt"], 1);
    rig.test
        .rig
        .wait_event(&lead.pane, cursor, drafted(&reference(&id)));
    watch.wait("the receipt", about("msg.delivered", &id));
    assert_eq!(rig.accepted_since(&lead.pane, cursor), [reference(&id)]);
    rig.finish_turn(&lead.pane);
    drop(client);
    rig.test.rig.close();
}

/// A recipient with a held turn, the next prompt hook set to be lost, and
/// these messages queued for it in order. Ending the turn starts the first.
fn queue_behind_lost_hook(rig: &MessageRig, bodies: &[&str]) -> (support::Recipient, Vec<String>) {
    let lead = rig.ready_agent("lead", StandInMode::Claude);
    rig.hold_turn(&lead.pane);
    rig.stand_in_control(&lead.pane, "/atm/drop-next UserPromptSubmit", "drop armed");
    let ids = bodies
        .iter()
        .map(|body| rig.send(None, &lead.id, body))
        .collect();
    (lead, ids)
}

/// The head's attempt: Enter lands, its hook is lost, its turn ends, and the
/// daemon holds it. Returns the owner's notice, which is the fence.
fn block_head(rig: &MessageRig, watch: &mut Watch, lead: &support::Recipient, head: &str) -> Value {
    let cursor = rig.test.rig.event_cursor(&lead.pane);
    rig.finish_turn(&lead.pane);
    watch.wait("the head's attempt", about("msg.delivery_attempted", head));
    rig.test
        .rig
        .wait_event(&lead.pane, cursor, accepted(&reference(head)));
    rig.finish_turn(&lead.pane);
    watch.wait("the blocked notice", notice(&format!("{head} blocked: ")))
}

/// The notice for a blocked message lists the messages queued behind it, and
/// none of those is typed while it is held.
#[test]
fn blocked_queue_notice_lists_followers() {
    let mut rig = MessageRig::start(Confirmation::Default);
    let mut watch = rig.watch();
    let (lead, ids) = queue_behind_lost_hook(&rig, &["head", "second", "third"]);
    let cursor = rig.test.rig.event_cursor(&lead.pane);
    let told = block_head(&rig, &mut watch, &lead, &ids[0]);

    let text = told["body"]["body"].as_str().unwrap();
    assert!(
        text.contains(&format!("waiting behind it: {}, {}. ", ids[1], ids[2])),
        "{text}"
    );
    // One notice for the head, none of their own for the followers.
    watch.catch_up(&rig);
    assert_eq!(watch.all(notice(&format!("{} blocked: ", ids[0]))).len(), 1);
    for follower in &ids[1..] {
        assert!(watch.all(notice(&format!("{follower} "))).is_empty());
        assert!(watch
            .all(about("msg.delivery_attempted", follower))
            .is_empty());
        let entry = rig.inbox_entry(None, Some(&lead.id), follower);
        assert_eq!(entry["status"], "queued", "{entry}");
        assert_eq!(entry["delivery"]["attempt"], 0, "{entry}");
        // History's predecessor is not the scheduler's veto. Check its own
        // decision so passing cannot depend on outrunning stable idle.
        assert_eq!(
            entry["delivery"]["reason"], "another delivery is in flight on this pane",
            "{entry}"
        );
        assert_ne!(entry["delivery"]["blocked_by"], json!(null), "{entry}");
    }
    let second = rig.inbox_entry(None, Some(&lead.id), &ids[1]);
    assert_eq!(second["delivery"]["blocked_by"], ids[0].as_str());
    // The recipient is idle with an empty editor: only the hold keeps them out.
    assert_eq!(rig.accepted_since(&lead.pane, cursor), [reference(&ids[0])]);
    rig.test.rig.close();
}

/// The owner gives up on the held message: it fails for good, and the
/// messages behind it are delivered in their order.
#[test]
fn give_up_releases_queue() {
    let mut rig = MessageRig::start(Confirmation::Default);
    let mut watch = rig.watch();
    let (lead, ids) = queue_behind_lost_hook(&rig, &["head", "second", "third"]);
    let cursor = rig.test.rig.event_cursor(&lead.pane);
    block_head(&rig, &mut watch, &lead, &ids[0]);

    let gave_up = rig.atm_json(None, &["msg", "give-up", &ids[0], "--format", "json"]);
    assert_eq!(gave_up, json!({"message_id": ids[0], "status": "failed"}));
    let failed = watch.wait("the final failure", about("msg.delivery_failed", &ids[0]));
    assert_eq!(failed["body"]["final"], true);
    assert_eq!(failed["body"]["reason"], "owner gave up on delivery");

    let mut order = vec![seq(&failed)];
    for follower in &ids[1..] {
        let attempted = watch.wait(
            "a follower's attempt",
            about("msg.delivery_attempted", follower),
        );
        assert_eq!(attempted["body"]["attempt"], 1);
        let delivered = watch.wait("a follower's receipt", about("msg.delivered", follower));
        order.extend([seq(&attempted), seq(&delivered)]);
        rig.finish_turn(&lead.pane);
    }
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    assert_eq!(
        rig.accepted_since(&lead.pane, cursor),
        [reference(&ids[0]), reference(&ids[1]), reference(&ids[2])]
    );
    assert_eq!(attempts(&mut watch, &rig, &ids[0]).len(), 1);
    rig.test.rig.close();
}

/// The owner's retry keeps the message: the same id, no new `msg.sent`, the
/// next attempt number, and its place before the messages behind it.
///
/// Not exercised here: a late receipt of the first attempt that arrives
/// after the retry. The stand-in can drop a hook but cannot hold one and
/// release it later; that part stays with atmd's scripted tests.
#[test]
fn owner_retry_same_id_keeps_fifo() {
    let mut rig = MessageRig::start(Confirmation::Default);
    let mut watch = rig.watch();
    let (lead, ids) = queue_behind_lost_hook(&rig, &["head", "follower"]);
    let cursor = rig.test.rig.event_cursor(&lead.pane);
    block_head(&rig, &mut watch, &lead, &ids[0]);

    let retry = ["msg", "retry", ids[0].as_str(), "--accept-duplicate-risk"];
    let retried = rig.atm_json(None, &[&retry[..], &["--format", "json"]].concat());
    assert_eq!(retried["message_id"], ids[0].as_str());
    let again = watch.wait(
        "the second attempt",
        about("msg.delivery_attempted", &ids[0]),
    );
    assert_eq!(again["body"]["attempt"], 2, "attempts never restart");
    let delivered = watch.wait("the head's receipt", about("msg.delivered", &ids[0]));
    rig.finish_turn(&lead.pane);
    let follower = watch.wait(
        "the follower's attempt",
        about("msg.delivery_attempted", &ids[1]),
    );
    assert!(seq(&delivered) < seq(&follower), "the head keeps its place");
    watch.wait("the follower's receipt", about("msg.delivered", &ids[1]));
    rig.finish_turn(&lead.pane);

    // The retry sent nothing new: two messages to the agent, as before.
    watch.catch_up(&rig);
    let to_agent = watch.all(|e| e["kind"] == "msg.sent" && e["body"]["to"] == lead.id.as_str());
    assert_eq!(
        to_agent.iter().map(id_of).collect::<Vec<_>>(),
        ids,
        "{to_agent:?}"
    );
    assert_eq!(
        rig.accepted_since(&lead.pane, cursor),
        [reference(&ids[0]), reference(&ids[0]), reference(&ids[1])],
        "the owner accepted the duplicate"
    );
    rig.test.rig.close();
}

/// A message with an unconfirmed attempt stays held across a daemon restart,
/// and the message queued behind it stays queued: the new daemon types
/// nothing for either.
///
/// The head's Enter lands and its hook is lost, so the old daemon's attempt
/// has ended in a hold before that daemon is stopped.
#[test]
fn restart_queued_and_delivering_hold() {
    let mut rig = MessageRig::start(Confirmation::Default);
    let mut watch = rig.watch();
    let (lead, ids) = queue_behind_lost_hook(&rig, &["head", "follower"]);
    let cursor = rig.test.rig.event_cursor(&lead.pane);
    rig.finish_turn(&lead.pane);
    watch.wait(
        "the head's attempt",
        about("msg.delivery_attempted", &ids[0]),
    );
    rig.test
        .rig
        .wait_event(&lead.pane, cursor, drafted(&reference(&ids[0])));
    // The old daemon's attempt is over once it has told the owner.
    watch.wait(
        "the blocked notice",
        notice(&format!("{} blocked: ", ids[0])),
    );
    assert_eq!(rig.accepted_since(&lead.pane, cursor), [reference(&ids[0])]);
    let end = rig.test.cursor();
    drop(watch);

    rig.test.rig.stop_daemon();
    let cursor = rig.test.rig.event_cursor(&lead.pane);
    let screen = rig.test.rig.capture_styled(&lead.pane);
    rig.test.rig.start_daemon();
    let mut watch = Watch::open(&rig, end);

    // The fence: the new daemon evaluated the head and held it for the owner.
    let restarted = "daemon restarted after an unconfirmed attempt";
    let told = watch.wait(
        "the restart notice",
        notice(&format!("{} blocked: {restarted}", ids[0])),
    );
    let text = told["body"]["body"].as_str().unwrap();
    assert!(
        text.contains(&format!("waiting behind it: {}. ", ids[1])),
        "{text}"
    );
    watch.catch_up(&rig);
    assert!(
        watch
            .all(|e| e["kind"] == "msg.delivery_attempted")
            .is_empty(),
        "an attempt after the restart: {:?}",
        watch.seen
    );
    assert!(watch.all(|e| e["kind"] == "msg.delivery_failed").is_empty());
    rig.assert_untouched(&lead.pane, cursor, &screen);
    let head = rig.inbox_entry(None, Some(&lead.id), &ids[0]);
    assert_eq!(head["status"], "delivering", "{head}");
    assert_eq!(head["delivery"]["attempt"], 1, "{head}");
    assert_eq!(head["delivery"]["phase"], "blocked", "{head}");
    let follower = rig.inbox_entry(None, Some(&lead.id), &ids[1]);
    assert_eq!(follower["status"], "queued", "{follower}");
    assert_eq!(follower["delivery"]["attempt"], 0, "{follower}");
    rig.test.rig.close();
}

/// A message that was never attempted is not held by a restart: the new
/// daemon attempts it as its first attempt once the recipient is free.
///
#[test]
#[ignore = "the daemon does not yet re-derive an adopted agent's baseline after restart"]
fn unattempted_restart_recovers() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", StandInMode::Claude);
    // The recipient is in a turn, so the message waits unattempted.
    rig.hold_turn(&lead.pane);
    let mut watch = rig.watch();
    let id = rig.send(None, &lead.id, "sent before the restart");
    watch.wait("the message", |e| e["kind"] == "msg.sent");
    assert!(attempts(&mut watch, &rig, &id).is_empty());
    let end = rig.test.cursor();
    drop(watch);

    rig.test.rig.restart_daemon();
    let mut watch = Watch::open(&rig, end);
    let cursor = rig.test.rig.event_cursor(&lead.pane);
    rig.finish_turn(&lead.pane);

    let attempted = watch.wait("the attempt", about("msg.delivery_attempted", &id));
    assert_eq!(attempted["body"]["attempt"], 1);
    let delivered = watch.wait("the receipt", about("msg.delivered", &id));
    assert_eq!(
        delivered["body"]["session_id"].as_str(),
        lead.session.as_deref()
    );
    assert_eq!(rig.accepted_since(&lead.pane, cursor), [reference(&id)]);
    rig.finish_turn(&lead.pane);
    // The only attempt in the whole log is the new daemon's.
    let log = rig.test.tail(0);
    let all: Vec<_> = log
        .iter()
        .filter(|e| e["kind"] == "msg.delivery_attempted")
        .collect();
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(seq(all[0]) > end);
    watch.catch_up(&rig);
    assert!(
        watch
            .all(notice(&format!("{id} blocked: daemon restarted")))
            .is_empty(),
        "no attempt, no restart hold"
    );
    rig.test.rig.close();
}

/// A message sent while its recipient is clearing is delivered to the new
/// session as its own prompt, never joined to the clear command.
///
/// On pi: after a clear the Claude stand-in shows its fresh frame, which is
/// never adopted Ready at 200 columns (it has no placeholder style proof).
#[test]
fn clear_then_message_not_merged() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", StandInMode::Pi);
    let mut watch = rig.watch();
    let cursor = rig.test.rig.event_cursor(&lead.pane);

    // The stand-in's manual clock holds the clear in its gap.
    let socket = rig.test.rig.socket();
    let params = json!({"agent": lead.id, "timeout_ms": 120_000});
    let clear = std::thread::spawn(move || support::call_on(&socket, "agent.clear", params, None));
    let requested = watch.wait("the clear request", |e| {
        e["kind"] == "agent.clear_requested"
    });
    rig.test.rig.wait_event(&lead.pane, cursor, |e| {
        e["kind"] == "rendered" && e["state"] == "clearing"
    });
    let id = rig.send(None, &lead.id, "for the new session");
    let sent = watch.wait("the message", |e| e["kind"] == "msg.sent");
    assert!(seq(&requested) < seq(&sent));

    rig.advance(&lead.pane, 100, "idle");
    let cleared = clear.join().unwrap().expect("agent.clear");
    let new = cleared["session_id"].as_str().expect("the new session");
    assert_ne!(Some(new), lead.session.as_deref());

    let delivered = watch.wait("the receipt", about("msg.delivered", &id));
    assert_eq!(delivered["body"]["session_id"], new);
    rig.test
        .rig
        .wait_event(&lead.pane, cursor, accepted(&reference(&id)));
    assert_eq!(rig.accepted_since(&lead.pane, cursor), [reference(&id)]);
    for record in rig.test.rig.journal(&lead.pane) {
        let text = record["text"].as_str().unwrap_or_default();
        assert!(
            !(text.contains("/new") && text.contains(&id)),
            "the clear command and the reference were joined: {record}"
        );
    }
    assert_eq!(attempts(&mut watch, &rig, &id).len(), 1);
    rig.finish_turn(&lead.pane);
    rig.test.rig.close();
}

/// A delivery with a lost prompt hook and a subsequent clear stay separate:
/// one accepted reference, no delivery failure, and the owner's hold notice.
///
/// On pi, for the reason given at `clear_then_message_not_merged`. The clear
/// is requested only after the reference was accepted. This does not prove
/// that the clear waits for an in-flight delivery reservation: the attempt
/// may already have ended. The stand-in can drop a hook but cannot hold one
/// and release it later to prove that overlap.
#[test]
fn held_delivery_then_clear_not_joined() {
    let mut rig = MessageRig::start(Confirmation::Default);
    let lead = rig.ready_agent("lead", StandInMode::Pi);
    rig.stand_in_control(&lead.pane, "/atm/drop-next input", "drop armed");
    let start = rig.test.cursor();
    let mut watch = Watch::open(&rig, start);
    let cursor = rig.test.rig.event_cursor(&lead.pane);

    let id = rig.send(None, &lead.id, "before the clear");
    watch.wait("the attempt", about("msg.delivery_attempted", &id));
    rig.test
        .rig
        .wait_event(&lead.pane, cursor, accepted(&reference(&id)));
    // The prompt was already accepted; no receipt can confirm it.
    let socket = rig.test.rig.socket();
    let params = json!({"agent": lead.id, "timeout_ms": 120_000});
    let clear = std::thread::spawn(move || support::call_on(&socket, "agent.clear", params, None));
    // The reference's turn ends; the clear may then type its command.
    rig.finish_turn(&lead.pane);
    watch.wait("the clear request", |e| {
        e["kind"] == "agent.clear_requested"
    });
    rig.test.rig.wait_event(&lead.pane, cursor, |e| {
        e["kind"] == "rendered" && e["state"] == "clearing"
    });
    rig.advance(&lead.pane, 100, "idle");
    clear.join().unwrap().expect("agent.clear");

    assert_eq!(rig.accepted_since(&lead.pane, cursor), [reference(&id)]);
    for record in rig.test.rig.journal(&lead.pane) {
        let text = record["text"].as_str().unwrap_or_default();
        assert!(
            !(text.contains("/new") && text.contains(&id)),
            "the reference and the clear command were joined: {record}"
        );
    }
    // The attempt is held for the owner; the clear released nothing. The
    // notice and the clear request are committed in either order, so it is
    // looked for from the start, on a watch of its own.
    Watch::open(&rig, start).wait("the blocked notice", notice(&format!("{id} blocked: ")));
    assert_eq!(attempts(&mut watch, &rig, &id).len(), 1);
    assert!(watch.all(about("msg.delivery_failed", &id)).is_empty());
    rig.test.rig.close();
}

/// A Codex agent that has no session yet is bound by the prompt that the
/// delivery itself submits, and the receipt names that session.
#[test]
#[ignore = "a fresh Codex stand-in at 200x50 is adopted Starting, never Ready and unbound: adoption finds no safe editor on its screen"]
fn lazy_codex_id_binds_only_after_prompt() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let pane = rig
        .test
        .shell_agent("lead", &lifecycle::config(StandInMode::Codex));
    rig.test.rig.wait_ready(&pane, None);
    let adopted = rig.test.adopt("lead", &pane);
    assert_eq!(adopted["state"], "ready", "{adopted}");
    let agent = adopted["agent_id"].as_str().unwrap().to_owned();
    // Codex announces nothing before its first prompt: no id to bind.
    assert_eq!(rig.test.agent(&agent)["session_id"], json!(null));
    let before = rig.test.tail(0);
    assert!(!before.iter().any(|e| e["kind"] == "agent.session_bound"));
    let mut watch = rig.watch();
    let cursor = rig.test.rig.event_cursor(&pane);

    let id = rig.send(None, &agent, "the first prompt of this session");
    let attempted = watch.wait("the attempt", about("msg.delivery_attempted", &id));
    rig.test
        .rig
        .wait_event(&pane, cursor, accepted(&reference(&id)));
    let delivered = watch.wait("the receipt", about("msg.delivered", &id));
    let session = delivered["body"]["session_id"].as_str().unwrap().to_owned();
    let row = rig.test.rig.wait_for_row(&pane, |r| !r.id.is_pending());
    assert_eq!(
        row.id.as_str(),
        session,
        "the receipt names the stand-in's id"
    );

    // The binding follows the prompt, and is the receipt's session.
    watch.catch_up(&rig);
    let bound = watch.all(|e| e["kind"] == "agent.session_bound");
    assert_eq!(bound.len(), 1, "{bound:?}");
    assert_eq!(bound[0]["body"]["session_id"], session.as_str());
    assert!(seq(&attempted) < seq(&bound[0]));
    assert_eq!(rig.test.agent(&agent)["session_id"], session.as_str());
    assert_eq!(rig.accepted_since(&pane, cursor), [reference(&id)]);
    rig.finish_turn(&pane);
    rig.test.rig.close();
}

/// When the organisation log cannot take the attempt's event, nothing is
/// pasted: the attempt is committed before any input, or there is no input.
///
/// The fault is real and outside the daemon: its log's segment is full and
/// its directory read-only, so the next append is refused before it is
/// written. The owner's focus holds the message until the fault is in place,
/// because looking away appends nothing: the attempt's own event is then the
/// first record the log refuses.
#[test]
fn unavailable_org_causes_no_paste() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let lead = rig.ready_agent("lead", StandInMode::Claude);
    let mut client = FocusClient::attach(&rig, &lead.pane);
    client.focus(&rig, true);
    let mut watch = rig.watch();
    let id = rig.send(None, &lead.id, "never typed without its attempt on record");
    let pane = &lead.pane.pane_id;
    watch.wait(
        "the focus notice",
        notice(&format!("{id} waiting: owner is focused on {pane}")),
    );
    assert!(attempts(&mut watch, &rig, &id).is_empty());
    drop(watch);

    let fault = support::RefusedAppends::start(&rig);
    let cursor = rig.test.rig.event_cursor(&lead.pane);
    let screen = rig.test.rig.capture_styled(&lead.pane);
    let refused = support::RefusedAppends::LOGGED;
    assert!(
        !rig.daemon_log().contains(refused),
        "another append met the fault before the attempt could"
    );
    client.focus(&rig, false);

    rig.test.rig.wait("the log's refusal of the attempt", || {
        rig.daemon_log().contains(refused)
    });
    // The actor logs its refusal before the executor handles the error. Fence
    // on the scheduler's return, after the executor and its cleanup are done.
    rig.test.rig.wait("the attempt's return", || {
        rig.daemon_log()
            .contains("Message scheduling round failed; no input")
    });
    let (code, message) = rig
        .call("msg.inbox", json!({"for": lead.id}), None)
        .expect_err("the message service still answers without its log");
    assert_eq!(code, "unavailable", "{message}");
    rig.assert_untouched(&lead.pane, cursor, &screen);
    fault.assert_nothing_appended();
    drop(fault);
    drop(client);
    rig.test.rig.close();
}
