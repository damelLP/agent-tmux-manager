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
fn told(
    rig: &MessageRig,
    watch: &mut Watch,
    seat: &Recipient,
    cursor: u64,
    kind: &str,
    body: &str,
) -> String {
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

fn status(rig: &MessageRig) -> String {
    String::from_utf8(rig.atm(None, &["status"]).stdout).unwrap()
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
    assert!(status(&rig).ends_with(" away\n"), "{:?}", status(&rig));

    // The agent asks through the CLI; the existing daemon policy takes it.
    let cursor = rig.test.rig.event_cursor(&seat.pane);
    let decision = rig.atm_json(
        Some(&seat.pane),
        &[
            "decisions",
            "request",
            "--kind",
            "plan",
            "--title",
            "Which interface",
            "--context",
            "Private question",
            "--option",
            "yes",
            "--option",
            "no",
            "--recommended",
            "yes",
            "--may-be-undone",
            "true",
            "--changes-intent",
            "true",
            "--format",
            "json",
        ],
    )["decision_id"]
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
    let notice = told(&rig, &mut watch, &seat, cursor, "answer", &notice_body);
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
    let mut back = rig.atm_json(None, &["back", "--format", "json"]);
    let digest = back.as_object_mut().unwrap().remove("digest").unwrap();
    assert_eq!(digest["counts"]["during_absence"], 1);
    assert_eq!(digest["counts"]["waiting_tasks"], 1);
    assert_eq!(digest["rows"].as_array().unwrap().len(), 4);
    assert_eq!(digest["absence"]["since"], since);
    assert_eq!(
        back,
        json!({"changed":true,"away_since":since,"provisional_count":1,"pending_count":0})
    );
    assert_eq!(assign(&rig, &second, &seat).unwrap_err(), held);
    let seconds = |value: &Value| {
        chrono::DateTime::parse_from_rfc3339(value.as_str().unwrap())
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    assert_eq!(
        String::from_utf8(rig.atm(None, &["back"]).stdout).unwrap(),
        format!("Not away. Showing the last absence: {since} to {until}.\n\
Relay seat: none\n\
Decided for you while away (1):\n\
{decision} Which interface = yes [taken_provisionally; by=system]\n  Recorded work: 1 started (0 accepted), 1 not started.\n  Overruling would reopen 1 task.\n  redo: {first} [assigned; direct] First on it\n  not started: {second} [open; direct] Second on it\n  Answer: atm decide {decision} <option>\n\
Relayed spoken answers: none.\n\
Earlier provisional decisions: none.\n\
Still waiting (1):\n\
{second} Second on it [open; stacking cap on {decision}]\n\
Numbers (all recorded time): 1 decision taken for you; 0 overruled.\n\
Tasks redone per overrule: none.\n\
Recorded links only: unlinked work and an unlinked task in progress are not included.\n",
            since=seconds(&since), until=seconds(&digest["absence"]["until"]), first=first.as_str().unwrap(), second=second.as_str().unwrap())
    );
    assert!(!status(&rig).contains("away"), "{:?}", status(&rig));

    // The owner's answer lifts the hold.
    let mut watch = rig.watch();
    let cursor = rig.test.rig.event_cursor(&seat.pane);
    let answer = rig.atm_json(None, &["decide", &decision, "yes", "--format", "json"]);
    assert_eq!(answer["decision"]["status"], "resolved");
    told(
        &rig,
        &mut watch,
        &seat,
        cursor,
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

// Break caught: CLI facts change the away classification, skip kind guards, or undo provisional work.
#[test]
fn cli_decision_classes_and_withdraw_reask_follow_the_away_policy() {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let seat = rig.ready_agent("worker", StandInMode::Claude);
    rig.atm_json(None, &["away", "--format", "json"]);
    let mut watch = rig.watch();
    let mut waiting = None;
    for (kind, undone, recommended, class, status) in [
        ("plan", "true", true, "detail", "taken_provisionally"),
        ("plan", "false", true, "irreversible", "pending"),
        ("action", "true", true, "detail", "pending"),
        ("plan", "true", false, "detail", "pending"),
    ] {
        let cursor = rig.test.rig.event_cursor(&seat.pane);
        let mut args = vec![
            "decisions",
            "request",
            "--kind",
            kind,
            "--title",
            "CLI away question",
            "--option",
            "yes",
            "--option",
            "no",
            "--may-be-undone",
            undone,
            "--changes-intent",
            "false",
            "--format",
            "json",
        ];
        if recommended {
            args.extend(["--recommended", "yes"]);
        }
        let result = rig.atm_json(Some(&seat.pane), &args);
        // U8 adds the question post's id: still IDs, no inferred policy state.
        assert_eq!(
            result.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["decision_id", "post_id"],
            "creation reports IDs, not inferred policy state"
        );
        let id = result["decision_id"].as_str().unwrap().to_owned();
        let (message_kind, body) = if status == "taken_provisionally" {
            ("answer", format!("Decision {id} is yours to decide: recorded as yes, your recommended option.\nThe owner is away and sees it when he is back."))
        } else if !recommended {
            ("note", format!("Decision {id} waits for the owner: he is away, and it has no recommended option.\nTo have it handled while he is away, withdraw it and ask again with a recommended option."))
        } else {
            ("note", format!("Decision {id} waits for the owner: he is away, and it cannot be undone or reaches outside.\nOnly work that depends on it waits. Carry on with other work."))
        };
        told(&rig, &mut watch, &seat, cursor, message_kind, &body);
        // U8: the notice is a reply in the question's thread.
        let step = if status == "taken_provisionally" {
            "taken"
        } else {
            "waits"
        };
        assert_eq!(
            rig.decision_thread(&id),
            [
                (seat.id.clone(), "asked".into(), vec!["owner".into()]),
                ("system".into(), step.into(), vec![seat.id.clone()]),
            ]
        );
        let entry = rig
            .call("decision.get", json!({"id":id}), Some(&seat.pane))
            .unwrap();
        assert_eq!(entry["decision"]["class"], class);
        assert_eq!(entry["decision"]["status"], status);
        if status == "taken_provisionally" {
            assert_eq!(entry["decision"]["provisional"]["by"], seat.id);
            let out = rig.atm(Some(&seat.pane), &["decisions", "withdraw", &id]);
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert!(String::from_utf8_lossy(&out.stderr).contains("cannot withdraw decision"));
        } else {
            assert_eq!(entry["decision"]["provisional"], Value::Null);
        }
        if !recommended {
            waiting = Some(id);
        }
    }
    let waiting = waiting.unwrap();
    let mut posts = rig.watch();
    assert_eq!(
        rig.atm_json(
            Some(&seat.pane),
            &["decisions", "withdraw", &waiting, "--format", "json"]
        )["changed"],
        true
    );
    // U8: a withdrawal is a reply that wakes nobody.
    posts.wait("the withdrawal's reply", |e| {
        e["kind"] == "channel.posted"
            && e["body"]["decision"] == json!({"decision_id":waiting,"step":"withdrawn"})
    });
    assert_eq!(
        rig.decision_thread(&waiting).last().unwrap(),
        &(
            "system".to_owned(),
            "withdrawn".to_owned(),
            Vec::<String>::new()
        )
    );
    let cursor = rig.test.rig.event_cursor(&seat.pane);
    let replacement = rig.atm_json(
        Some(&seat.pane),
        &[
            "decisions",
            "request",
            "--kind",
            "plan",
            "--title",
            "CLI away question",
            "--option",
            "yes",
            "--option",
            "no",
            "--may-be-undone",
            "true",
            "--changes-intent",
            "false",
            "--recommended",
            "yes",
            "--format",
            "json",
        ],
    );
    let id = replacement["decision_id"].as_str().unwrap();
    assert_ne!(id, waiting);
    told(&rig, &mut watch, &seat, cursor, "answer", &format!("Decision {id} is yours to decide: recorded as yes, your recommended option.\nThe owner is away and sees it when he is back."));
    let entry = rig
        .call("decision.get", json!({"id":id}), Some(&seat.pane))
        .unwrap();
    assert_eq!(entry["decision"]["status"], "taken_provisionally");
    assert_eq!(entry["decision"]["provisional"]["by"], seat.id);
    rig.test.rig.close();
}
