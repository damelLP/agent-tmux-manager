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
    let test = &rig.test;
    let log = test.rig.root().join("lead.jsonl");
    let config = test.rig.root().join("lead.json");
    std::fs::write(
        &config,
        json!({
            "LOG": log, "ID_PREFIX": "lead", "MANUAL_CLOCK": 1,
            "WORK_MS": 500, "CLEAR_GAP_MS": 100, "SCREEN_VARIANT": "lead-shells"
        })
        .to_string(),
    )
    .unwrap();
    lifecycle::checked(
        test.rig
            .tmux(&["set-option", "-g", "default-size", "144x80"]),
    );
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
    let before = rig.test.rig.event_cursor(pane);
    let answer = rig.test.rig.control(
        pane,
        json!({
            "command": "render_source", "key": format!("claude.{phase}.lead-shells")
        }),
    );
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
    let restored = rig.test.agent(&id);
    assert_eq!(restored["state"], "ready");
    assert_eq!(restored["reason"], serde_json::Value::Null, "{restored}");
    assert_eq!(restored["activity"], serde_json::Value::Null);
    assert_eq!(
        rig.inbox_entry(None, Some(&id), &message)["delivery"]["attempt"],
        0
    );
    let mut watch = support::Watch::open(&rig, 0);
    // Announce the existing UUID with a status event, not a model prompt.
    rig.stand_in_control(&pane, "/atm/status", "status sent");
    let delivered = watch.wait(
        "queued reference after the real row",
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
        first["kind"], "status",
        "the fixture announced identity without a model prompt"
    );
    rig.test.rig.close();
}
