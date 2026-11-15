//! The away switch on a real private daemon and tmux, with a model-free
//! stand-in as the agent; never a paid CLI. Every wait is on a committed
//! event or on the stand-in's own journal.
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
use support::{about, reference, Confirmation, MessageRig, Recipient, Watch};

/// The next system message to the seat: its id, after its body was checked,
/// its reference line reached the stand-in and nothing else was typed.
fn told(rig: &MessageRig, watch: &mut Watch, seat: &Recipient, kind: &str, body: &str) -> String {
    let cursor = rig.test.rig.event_cursor(&seat.pane);
    let record = watch.wait("a message to the seat", |e| {
        e["kind"] == "msg.sent" && e["body"]["to"] == seat.id
    });
    assert_eq!(record["body"]["kind"], kind, "{record}");
    assert_eq!(record["body"]["body"], body, "{record}");
    let id = format!("m{}", record["seq"]);
    watch.wait("its confirmed delivery", about("msg.delivered", &id));
    let line = reference(&id);
    rig.test.rig.wait_event(&seat.pane, cursor, |e| {
        e["kind"] == "accepted" && e["text"] == line
    });
    rig.finish_turn(&seat.pane);
    id
}

fn assign(rig: &MessageRig, task: &Value, seat: &Recipient) -> Result<Value, (String, String)> {
    rig.call(
        "task.assign",
        json!({"task_id":task,"agent_id":seat.id}),
        None,
    )
}

#[test]
fn a_decision_asked_while_away_is_taken_told_capped_and_survives_a_restart() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    // The Claude stand-in, as the existing restart-delivery test uses: an
    // adopted pi stand-in stayed `starting` after the daemon's restart here,
    // which holds every later delivery and is not this unit's to change.
    let seat = rig.ready_agent("worker", StandInMode::Claude);
    let settings = rig.test.project.join(".atm/away.toml");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();

    // A broken settings file refuses the switch, with the file's path.
    std::fs::write(&settings, "stacking_cap = 0\n").unwrap();
    let refused = rig.atm(None, &["away"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(refused.stdout.is_empty(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains(".atm/away.toml: stacking_cap must be at least 1"),
        "{refused:?}"
    );
    std::fs::write(&settings, "stacking_cap = 2\n").unwrap();

    // The agent may not set it; the owner may, and the cap comes from the file.
    let agent = rig.atm(Some(&seat.pane), &["away"]);
    assert!(!agent.status.success(), "{agent:?}");
    assert!(
        String::from_utf8_lossy(&agent.stderr).contains("only the owner may set away or back"),
        "{agent:?}"
    );
    let mut watch = rig.watch();
    let away = rig.atm_json(None, &["away", "--format", "json"]);
    assert_eq!(away["changed"], true);
    assert_eq!(away["stacking_caps"], json!({"project":2}));
    assert_eq!(away["pending_count"], 0);
    let since = away["away_since"].clone();
    assert!(since.is_string());

    // The agent asks; the daemon takes it for the owner and tells the agent.
    let decision = rig
        .call(
            "decision.request",
            json!({"kind":"plan","title":"Which interface","context":"Private question","options":["yes","no"],"recommended":"yes","may_be_undone":true,"changes_intent":true}),
            Some(&seat.pane),
        )
        .unwrap()["decision_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let take = watch.wait("the provisional take", |e| {
        e["kind"] == "decision.taken_provisionally"
    });
    assert_eq!(
        take["body"],
        json!({"decision_id":decision,"option":"yes","by":"system"})
    );
    let notice_body = format!("Decision {decision} taken provisionally: yes\nThe owner is away. Finish your task in progress on this option. Start nothing further on it: a task built on it names {decision} in rests_on and waits until the owner has seen {decision}.");
    let notice = told(&rig, &mut watch, &seat, "answer", &notice_body);
    assert_eq!(
        rig.inbox_entry(Some(&seat.pane), None, &notice)["content"]["body"],
        notice_body.as_str()
    );

    // Two further tasks are written down on it. The cap of two lets one start.
    let task = |title: &str| {
        rig.call(
            "task.create",
            json!({"title":title,"measure":{"kind":"owner"},"rests_on":decision}),
            None,
        )
        .unwrap()["task_id"]
            .clone()
    };
    let (first, second) = (task("First on it"), task("Second on it"));
    assert_eq!(assign(&rig, &first, &seat).unwrap()["changed"], true);
    let brief = watch.wait("the brief of the first", |e| {
        e["kind"] == "msg.sent" && e["body"]["kind"] == "brief"
    });
    let brief = format!("m{}", brief["seq"]);
    watch.wait("the brief's delivery", about("msg.delivered", &brief));
    rig.finish_turn(&seat.pane);
    let held = (
        "refused".to_owned(),
        format!("task {} rests on decision {decision}, which was taken provisionally for the owner: it waits until he has seen {decision} (stacking cap 2)", second.as_str().unwrap()),
    );
    assert_eq!(assign(&rig, &second, &seat).unwrap_err(), held);

    // A restart keeps the switch, the take, the cap and the one notice. The
    // seat is in a turn across it: the end of that turn is what tells the new
    // daemon of its session, as a harness's next hook would.
    rig.hold_turn(&seat.pane);
    rig.test.rig.restart_daemon();
    rig.finish_turn(&seat.pane);
    let again = rig.atm_json(None, &["away", "--format", "json"]);
    assert_eq!(again["changed"], false);
    assert_eq!(again["away_since"], since);
    assert_eq!(again["stacking_caps"], json!({"project":2}));
    assert_eq!(
        rig.call("decision.get", json!({"id":decision}), None)
            .unwrap()["decision"]["status"],
        "taken_provisionally"
    );
    let inbox = rig.call("msg.inbox", json!({"for":seat.id}), None).unwrap();
    assert_eq!(
        inbox["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["message_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        [notice, brief],
        "a restart must not send the notice again"
    );
    assert_eq!(assign(&rig, &second, &seat).unwrap_err(), held);

    // Back is not an answer: the second task still waits.
    let back = rig.atm_json(None, &["back", "--format", "json"]);
    assert_eq!(
        back,
        json!({"changed":true,"away_since":since,"provisional_count":1,"pending_count":0})
    );
    assert_eq!(assign(&rig, &second, &seat).unwrap_err(), held);
    assert_eq!(
        String::from_utf8(rig.atm(None, &["back"]).stdout).unwrap(),
        "Not away.\n"
    );

    // The owner's answer lifts the hold.
    let mut watch = rig.watch();
    let answer = rig.atm_json(None, &["decide", &decision, "yes", "--format", "json"]);
    assert_eq!(answer["decision"]["status"], "resolved");
    told(
        &rig,
        &mut watch,
        &seat,
        "answer",
        &format!("Decision {decision} resolved: yes\nStanding rule: no"),
    );
    assert_eq!(assign(&rig, &second, &seat).unwrap()["changed"], true);
    let brief = watch.wait("the brief of the second", |e| {
        e["kind"] == "msg.sent" && e["body"]["kind"] == "brief"
    });
    watch.wait(
        "its delivery",
        about("msg.delivered", &format!("m{}", brief["seq"])),
    );
    rig.finish_turn(&seat.pane);
    rig.test.rig.close();
}
