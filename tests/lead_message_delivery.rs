//! The captured lead chrome through real private tmux, daemon and delivery.
//! No model CLI runs. All resources and process identities belong to StandInRig.
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
use stand_in::{StandInMode, StandInPane};
use support::{about, reference, Confirmation, MessageRig};

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn native_lead(rig: &MessageRig) -> StandInPane {
    native_claude(rig, "144x80", "lead-shells")
}

fn native_claude(rig: &MessageRig, size: &str, variant: &str) -> StandInPane {
    let test = &rig.test;
    let log = test.rig.root().join("lead.jsonl");
    let config = test.rig.root().join("lead.json");
    std::fs::write(
        &config,
        json!({
            "LOG": log, "ID_PREFIX": "lead", "MANUAL_CLOCK": 1,
            "WORK_MS": 500, "CLEAR_GAP_MS": 100, "SCREEN_VARIANT": variant
        })
        .to_string(),
    )
    .unwrap();
    lifecycle::checked(test.rig.tmux(&["set-option", "-g", "default-size", size]));
    // Unlike the noninteractive default launcher, bash -m places the harness
    // in its own foreground job. Its root remains alive until rig cleanup.
    let job = format!(
        "{} --atm-config {}; exec /bin/bash --noprofile --norc",
        quote(test.rig.launcher(StandInMode::Claude).to_str().unwrap()),
        quote(config.to_str().unwrap())
    );
    let command = format!("/bin/bash --noprofile --norc -m -c {}", quote(&job));
    let id = lifecycle::checked(test.rig.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-c",
        test.project.to_str().unwrap(),
        &command,
    ]));
    let pane = test.rig.observe_pane(id, StandInMode::Claude, log);
    test.rig.wait_event(&pane, 0, |e| e["kind"] == "rendered");
    pane
}

fn replay(rig: &MessageRig, pane: &StandInPane, phase: &str) {
    replay_source(rig, pane, &format!("claude.{phase}.lead-shells"));
}

fn replay_source(rig: &MessageRig, pane: &StandInPane, key: &str) {
    let before = rig.test.rig.event_cursor(pane);
    let answer = rig
        .test
        .rig
        .control(pane, json!({"command": "render_source", "key": key}));
    assert!(answer.get("error").is_none(), "{answer}");
    rig.test.rig.wait_event(pane, before, |e| {
        e["kind"] == "rendered" && e["state"] == "source-replay"
    });
}

#[test]
fn lead_working_chrome_blocks_even_when_the_registry_is_idle() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let pane = native_lead(&rig);
    rig.test.complete_fixture_turn(&pane);
    let adopted = rig.test.adopt("lead", &pane);
    assert_eq!(adopted["state"], "ready");
    let id = adopted["agent_id"].as_str().unwrap();
    replay(&rig, &pane, "working");
    let before = rig.test.rig.event_cursor(&pane);
    let screen = rig.test.rig.capture_styled(&pane);
    let mut watch = rig.watch();
    let message = rig.send(None, id, "the body never goes to the terminal");
    rig.test.rig.wait("working screen holds delivery", || {
        rig.inbox_entry(None, Some(id), &message)["delivery"]["phase"] == "waiting"
    });
    let held = rig.inbox_entry(None, Some(id), &message);
    assert_eq!(held["delivery"]["attempt"], 0, "{held}");
    assert_eq!(
        rig.test.agent(id)["activity"],
        "idle",
        "the screen, not the row, refuses"
    );
    rig.assert_untouched(&pane, before, &screen);
    replay(&rig, &pane, "idle");
    watch.wait(
        "exact receipt after idle chrome",
        about("msg.delivered", &message),
    );
    assert_eq!(
        rig.inbox_entry(None, Some(id), &message)["delivery"]["confirmation"],
        "prompt_submit_exact_reference"
    );
    assert_eq!(rig.accepted_since(&pane, before), [reference(&message)]);
    rig.test.rig.close();
}

#[test]
fn background_agent_list_holds_delivery_until_an_ordinary_idle_frame() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let pane = native_claude(&rig, "189x40", "u5");
    rig.test.complete_fixture_turn(&pane);
    let adopted = rig.test.adopt("lead", &pane);
    assert_eq!(adopted["state"], "ready");
    let id = adopted["agent_id"].as_str().unwrap();
    let term = &rig.test.rig;
    replay_source(&rig, &pane, "claude.background-agent-list");
    let held_screen = term.capture_styled(&pane);
    let last_row = held_screen.lines().last().unwrap();
    assert!(last_row.contains("◯ general-purpose"), "{last_row:?}");
    assert!(
        last_row.contains("1m 23s · ↓ 123.4k tokens"),
        "{last_row:?}"
    );
    let after = term.event_cursor(&pane);
    lifecycle::checked(term.tmux(&[
        "set-hook",
        "-g",
        "after-capture-pane",
        "wait-for -S background-agent-list-read",
    ]));
    let mut watch = rig.watch();
    let message = rig.send(None, id, "deliver only after the agent list is closed");
    // Each wake is a real pane capture. Use the first scheduler verdict, not
    // a timed retry for the desired string (all-Unknown must fail on RED).
    let held = (0..12)
        .find_map(|_| {
            lifecycle::checked(term.tmux(&["wait-for", "background-agent-list-read"]));
            let entry = rig.inbox_entry(None, Some(id), &message);
            (entry["delivery"]["phase"] != "queued").then_some(entry)
        })
        .expect("daemon did not record a delivery verdict after captures");
    assert_eq!(held["delivery"]["phase"], "waiting", "{held}");
    assert_eq!(held["delivery"]["attempt"], 0, "{held}");
    assert!(
        held["delivery"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("atm cannot read the turn state on "),
        "the screen should be readable, with an unknown turn: {held}"
    );
    lifecycle::checked(term.tmux(&["set-hook", "-gu", "after-capture-pane"]));
    rig.assert_untouched(&pane, after, &held_screen);
    assert!(watch
        .all(about("msg.delivery_attempted", &message))
        .is_empty());

    replay_source(&rig, &pane, "claude.idle.u5");
    watch.wait(
        "one attempt after the list closes",
        about("msg.delivery_attempted", &message),
    );
    watch.wait("the exact prompt receipt", about("msg.delivered", &message));
    let delivered = rig.inbox_entry(None, Some(id), &message);
    assert_eq!(delivered["delivery"]["attempt"], 1, "{delivered}");
    assert_eq!(
        delivered["delivery"]["confirmation"],
        "prompt_submit_exact_reference"
    );
    assert_eq!(rig.accepted_since(&pane, after), [reference(&message)]);
    rig.test.rig.close();
}

#[test]
fn lead_restart_repins_before_its_real_session_row_is_announced() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let pane = native_lead(&rig);
    let session = rig.test.complete_fixture_turn(&pane).id.to_string();
    let adopted = rig.test.adopt("lead", &pane);
    assert_eq!(adopted["state"], "ready");
    let id = adopted["agent_id"].as_str().unwrap().to_owned();
    rig.hold_turn(&pane);
    let message = rig.send(None, &id, "queued before the daemon restart");
    rig.test.rig.wait("busy recipient holds delivery", || {
        rig.inbox_entry(None, Some(&id), &message)["delivery"]["phase"] == "waiting"
    });
    rig.test.rig.stop_daemon();
    // End the turn while the daemon is absent. Its initial observer can recover
    // the foreground pin from the idle frame before a real session row arrives.
    rig.finish_turn(&pane);
    let before = rig.test.rig.event_cursor(&pane);
    rig.test.rig.start_daemon();
    // The stand-in has no real Claude transcript for discovery to resolve its
    // UUID: only a pending row. Baseline repair must not claim that identity.
    // (Before s3-d1 the row also had to be announced before any delivery: a
    // status event was sent here. Now the pending row is the bound session's
    // own process, recorded at the adopt, so nothing is announced and the
    // queued message must still arrive; whether it is attempted before this
    // check depends on timing, so neither the attempt nor the live activity
    // is asserted here.)
    let restored = rig.test.agent(&id);
    assert_eq!(restored["state"], "ready");
    assert_eq!(restored["reason"], serde_json::Value::Null, "{restored}");
    assert_eq!(restored["session_id"], session.as_str(), "{restored}");
    let mut watch = support::Watch::open(&rig, 0);
    let delivered = watch.wait(
        "queued reference on the discovery row",
        about("msg.delivered", &message),
    );
    assert_eq!(delivered["body"]["session_id"], session);
    let entry = rig.inbox_entry(None, Some(&id), &message);
    assert_eq!(
        entry["delivery"]["confirmation"],
        "prompt_submit_exact_reference"
    );
    assert_eq!(entry["delivery"]["attempt"], 1);
    assert_eq!(rig.accepted_since(&pane, before), [reference(&message)]);
    let first = rig
        .test
        .rig
        .journal(&pane)
        .into_iter()
        .find(|e| {
            e["event_sequence"].as_u64().unwrap() > before
                && matches!(e["kind"].as_str(), Some("event" | "status" | "accepted"))
        })
        .unwrap();
    assert_eq!(
        first["kind"], "accepted",
        "delivered on the discovery row, before the fixture announced anything"
    );
    rig.test.rig.close();
}

#[test]
fn pending_only_readopt_refuses_in_a_solo_window() {
    pending_only_readopt_refuses(false);
}

#[test]
fn pending_only_readopt_refuses_without_resizing_a_split_window() {
    pending_only_readopt_refuses(true);
}

fn pending_only_readopt_refuses(split: bool) {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let pane = native_lead(&rig);
    let term = &rig.test.rig;
    let target = pane.pane_id.as_str();
    if split {
        // Widen the window before adding its sibling, keeping the lead at 144
        // columns as drawn by this fixture's fixed-size renderer.
        lifecycle::checked(term.tmux(&["resize-window", "-t", target, "-x", "177"]));
        lifecycle::checked(term.tmux(&[
            "split-window",
            "-d",
            "-h",
            "-l",
            "32",
            "-t",
            target,
            "/bin/sh",
        ]));
    }
    let session = rig.test.complete_fixture_turn(&pane).id;
    let first = rig.test.adopt("lead", &pane);
    let id = first["agent_id"].as_str().unwrap();
    assert_eq!(first["state"], "ready");
    assert_eq!(rig.test.agent(id)["session_id"], session.as_str());
    assert_eq!(rig.test.adopt("lead", &pane), first, "live binding control");

    rig.test.rig.restart_daemon();
    let bound = rig.test.agent(id);
    assert_eq!(bound["state"], "ready");
    assert_eq!(bound["session_id"], session.as_str());
    let pending = rig.test.rig.wait_for_row(&pane, |row| row.id.is_pending());
    let cursor = rig.test.cursor();
    let term = &rig.test.rig;
    // A layout readback alone would miss a shrink followed by an undo. Have
    // private tmux mark any resize-pane command, even one leaving no delta.
    const RESIZED: &str = "ATM_FIXTURE_RESIZED";
    lifecycle::checked(term.tmux(&[
        "set-hook",
        "-g",
        "after-resize-pane",
        "set-environment -g ATM_FIXTURE_RESIZED yes",
    ]));
    let resize_marker = || lifecycle::checked(term.tmux(&["show-environment", "-g", RESIZED]));
    lifecycle::checked(term.tmux(&["resize-pane", "-t", target, "-x", "144"]));
    assert_eq!(resize_marker(), "ATM_FIXTURE_RESIZED=yes");
    lifecycle::checked(term.tmux(&["set-environment", "-g", RESIZED, "no"]));
    let geometry = || {
        lifecycle::checked(term.tmux(&["display-message", "-p", "-t", target, "#{window_layout}"]))
    };
    let tag =
        || lifecycle::checked(term.tmux(&["show-options", "-p", "-v", "-t", target, "@atm-agent"]));
    let layout = geometry();
    assert_eq!(tag(), id);
    let before = term.event_cursor(&pane);
    let screen = term.capture_styled(&pane);
    let refusal = rig.call("agent.adopt", json!({
        "project_root": rig.test.project, "name": "lead", "pane": target, "caller_server": rig.test.server(),
    }), None);
    assert_eq!(refusal, Err(("refused".into(), format!(
        "agent {id} is already adopted in {target}; its harness has not reported its session since the restart, \
         so nothing was changed. It reports at the seat's next turn; a Claude seat also reports when its pane is resized."
    ))));
    assert_eq!(resize_marker(), "ATM_FIXTURE_RESIZED=no");
    assert_eq!(geometry(), layout);
    assert_eq!(tag(), id);
    assert_eq!(rig.test.agent(id), bound);
    let rows = term.pane_rows(&pane);
    let [row] = rows.as_slice() else {
        panic!("expected one discovery-only row: {rows:?}")
    };
    assert_eq!(row.id, pending.id);
    assert!(
        rig.test.tail(cursor).is_empty(),
        "the refused adopt wrote to the log"
    );
    rig.assert_untouched(&pane, before, &screen);
    // s3-d1: the discovery row is the bound session's own process (the adopt
    // recorded it), so a send is queued and delivered to that session with no
    // harness event before it. Until s3-d1 it was refused as unconfirmed.
    let mut watch = rig.watch();
    let message = rig.send(None, id, "queued on the discovery row");
    let delivered = watch.wait(
        "delivery while the row is a discovery row",
        about("msg.delivered", &message),
    );
    assert_eq!(delivered["body"]["session_id"], session.as_str());
    assert_eq!(
        delivered["body"]["confirmation"],
        "prompt_submit_exact_reference"
    );
    assert_eq!(rig.accepted_since(&pane, before), [reference(&message)]);
    rig.test.rig.close();
}
