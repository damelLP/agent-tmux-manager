//! The reminder on a silent turn end, on a private real daemon with stand-in
//! harnesses: no model CLI runs. The daemon's `[reminder]` delay is one
//! second; every step waits on a committed event or on the stand-in's own
//! events, and the stand-in's turns end on its manual clock.
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
use atm_core::SessionStatus;
use serde_json::{json, Value};
use stand_in::StandInMode;
use support::{about, reference, Confirmation, MessageRig, Recipient};

/// The message rig, with reminders one second after a silent turn end.
fn start() -> MessageRig {
    let mut rig = MessageRig::start(Confirmation::Generous);
    let config = rig.test.rig.root().join("config/atm/config.toml");
    let mut settings = std::fs::read_to_string(&config).unwrap();
    settings.push_str("[reminder]\ndelay_secs = 1\n");
    std::fs::write(&config, settings).unwrap();
    // The daemon reads its config once, at start.
    rig.test.rig.restart_daemon();
    assert_eq!(rig.test.rpc("agent.list", json!({}), None), json!([]));
    // Fails closed: an invalid section would mean no reminders at all.
    assert!(
        !rig.daemon_log().contains("Invalid reminder configuration"),
        "{}",
        rig.daemon_log()
    );
    rig
}

/// A task the lead creates and assigns to the worker. Returns the task and
/// the brief's message id, once the brief is typed: the worker's turn runs.
fn assigned(
    rig: &MessageRig,
    watch: &mut support::Watch,
    lead: &Recipient,
    worker: &Recipient,
) -> (String, String) {
    let created = rig
        .call(
            "task.create",
            json!({"title":"Quiet work","measure":{"kind":"owner"}}),
            Some(&lead.pane),
        )
        .unwrap();
    let task = created["task_id"].as_str().unwrap().to_owned();
    let assigned = rig
        .call(
            "task.assign",
            json!({"task_id":task,"agent_id":worker.id}),
            Some(&lead.pane),
        )
        .unwrap();
    let brief = assigned["notification"]["message_id"]
        .as_str()
        .unwrap()
        .to_owned();
    watch.wait("the brief is typed", about("msg.delivered", &brief));
    (task, brief)
}

/// The reminder's audit event and its note for `task`, in that order.
/// Returns the note's message id.
fn reminded(watch: &mut support::Watch, task: &str, worker: &Recipient, told: &str) -> String {
    let audit = watch.wait("the reminder's audit event", |e| {
        e["kind"] == "task.reminded" && e["body"]["task_id"] == task
    });
    assert_eq!(audit["body"]["agent_id"], worker.id.as_str());
    let note = watch.wait("the reminder", |e| {
        e["kind"] == "msg.sent"
            && e["body"]["from"] == "system"
            && e["body"]["to"] == worker.id.as_str()
    });
    assert_eq!(
        note["seq"].as_u64(),
        audit["seq"].as_u64().map(|seq| seq + 1),
        "the audit event is written first"
    );
    assert_eq!(
        note["body"],
        json!({"from":"system","to":worker.id,"kind":"note","attachments":[],"task":task,"body":format!(
            "Your turn ended and task {task} (\"Quiet work\") is still assigned.\n\
             Before you stop, say where it stands: atm task submit {task}, or atm task set-state \
             {task} waiting --reason '...' (or blocked). If you are still working, carry on; if \
             you have updated it since, ignore this. You get this once: if your next turn also \
             ends this way, {told} is told."
        )})
    );
    format!("m{}", note["seq"])
}

// Break caught: the daemon does not start the scheduler, reminds a running
// turn, reminds twice, or tells the wrong address.
#[test]
fn a_silent_turn_end_is_reminded_and_the_next_is_told_to_the_creator() {
    let mut rig = start();
    let lead = rig.ready_agent("lead", StandInMode::Pi);
    let worker = rig.ready_agent("worker", StandInMode::Pi);
    let mut watch = rig.watch();
    let worker_cursor = rig.test.rig.event_cursor(&worker.pane);
    let lead_cursor = rig.test.rig.event_cursor(&lead.pane);
    let (task, brief) = assigned(&rig, &mut watch, &lead, &worker);

    // The brief's turn ends and the task still says `assigned`.
    rig.finish_turn(&worker.pane);
    let reminder = reminded(&mut watch, &task, &worker, &lead.id);
    watch.wait("the reminder is typed", about("msg.delivered", &reminder));
    rig.test.rig.wait_event(&worker.pane, worker_cursor, |e| {
        e["kind"] == "accepted" && e["text"] == reference(&reminder)
    });
    assert_eq!(
        rig.accepted_since(&worker.pane, worker_cursor),
        [reference(&brief), reference(&reminder)]
    );

    // The turn the reminder started ends silent too.
    rig.finish_turn(&worker.pane);
    let flagged = watch.wait("the flag's audit event", |e| {
        e["kind"] == "task.flagged" && e["body"]["task_id"] == task.as_str()
    });
    assert_eq!(flagged["body"]["to"], lead.id.as_str());
    let told = watch.wait("the note to the creator", |e| {
        e["kind"] == "msg.sent"
            && e["body"]["from"] == "system"
            && e["body"]["to"] == lead.id.as_str()
    });
    let body = told["body"]["body"].as_str().unwrap();
    let start = format!(
        "Task {task} (\"Quiet work\"): {} ended two turns without moving it from assigned; \
         reminded at ",
        worker.id
    );
    assert!(body.starts_with(&start), "{body}");
    assert!(body.ends_with(&format!(". atm task get {task}")), "{body}");
    assert_eq!(told["body"]["task"], task.as_str());
    let note = format!("m{}", told["seq"]);
    watch.wait("the note is typed", about("msg.delivered", &note));
    rig.test.rig.wait_event(&lead.pane, lead_cursor, |e| {
        e["kind"] == "accepted" && e["text"] == reference(&note)
    });

    // Once each, and the task carries both marks.
    rig.finish_turn(&lead.pane);
    let got = rig.call("task.get", json!({"task_id":task}), None).unwrap();
    assert_eq!(got["task"]["state"], "assigned");
    assert_ne!(got["task"]["reminded_at"], Value::Null);
    assert_ne!(got["task"]["flagged_at"], Value::Null);
    watch.catch_up(&rig);
    for kind in ["task.reminded", "task.flagged"] {
        assert_eq!(watch.all(|e| e["kind"] == kind).len(), 1, "{kind}");
    }
    rig.test.rig.close();
}

// Break caught: a reminder decided while the row wrongly reads idle is typed
// into the turn that is still running.
//
// The stand-in holds the hook that would tell the daemon a turn has started
// (`hold_next_hook`, `release_hook`): the row reads idle while the screen
// shows the turn. Written against the controls' contract; never run before
// they exist.
#[test]
#[ignore = "needs cp/s1-standin"]
fn a_reminder_decided_while_the_row_wrongly_reads_idle_is_typed_after_the_turn() {
    let mut rig = start();
    let lead = rig.ready_agent("lead", StandInMode::Pi);
    let worker = rig.ready_agent("worker", StandInMode::Claude);
    let mut watch = rig.watch();
    let (task, _brief) = assigned(&rig, &mut watch, &lead, &worker);
    let cursor = rig.test.rig.event_cursor(&worker.pane);

    // A prompt typed while the brief's turn runs is queued by the stand-in
    // and starts when that turn ends. Its own hook will be held, so the
    // daemon sees the brief's turn end and no turn start.
    let armed = rig.test.rig.control(
        &worker.pane,
        json!({"command":"hold_next_hook","event":"UserPromptSubmit"}),
    );
    assert!(armed["error"].is_null(), "{armed}");
    rig.test.rig.paste(&worker.pane, "queued fixture turn");
    rig.test
        .rig
        .wait_event(&worker.pane, cursor, |e| e["kind"] == "queued");
    rig.advance(&worker.pane, 500, "working");
    rig.test.rig.wait_event(&worker.pane, cursor, |e| {
        e["kind"] == "held" && e["event"] == "UserPromptSubmit"
    });
    rig.test
        .rig
        .wait_for_row(&worker.pane, |row| row.status == SessionStatus::Idle);

    // The row has read idle for the delay: the reminder is decided.
    let reminder = reminded(&mut watch, &task, &worker, &lead.id);
    // Runtime progress has no committed event for a held message. Use the
    // rig's bounded condition poll, not a delay, as evidence.
    rig.test
        .rig
        .wait("the message runtime saw the running turn", || {
            let entry = rig.inbox_entry(Some(&worker.pane), None, &reminder);
            entry["delivery"]["phase"] == "waiting"
                && entry["delivery"]["reason"] == "session is busy"
        });
    assert!(!rig
        .accepted_since(&worker.pane, cursor)
        .contains(&reference(&reminder)));
    watch.catch_up(&rig);
    assert!(watch
        .all(about("msg.delivery_attempted", &reminder))
        .is_empty());

    // The hook arrives late, the turn ends, and only then is the line typed.
    let released = rig
        .test
        .rig
        .control(&worker.pane, json!({"command":"release_hook"}));
    assert!(released["error"].is_null(), "{released}");
    rig.finish_turn(&worker.pane);
    watch.wait("the reminder is typed", about("msg.delivered", &reminder));
    rig.test.rig.wait_event(&worker.pane, cursor, |e| {
        e["kind"] == "accepted" && e["text"] == reference(&reminder)
    });
    rig.finish_turn(&worker.pane);
    rig.test.rig.close();
}
