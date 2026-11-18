//! Real-daemon acceptance of the handover the daemon runs by itself, on the
//! shared, model-free stand-in: a seat whose role file says
//! `auto_handover = ["context"]` ends a turn above its context line, and
//! with no command the save request travels through the message runtime
//! into the stand-in's editor. The test plays the agent (it writes the
//! memory file and lets the turn end) and the daemon clears the session with
//! the real `agent.clear`. Every wait is on a stand-in event, a registry row
//! or a committed event.
//!
//! Synthetic: the context share is the stand-in's `set_context_share`
//! control, not a captured usage. The daemon's share is an f64 quotient (63
//! can arrive as 62.99999999999999), so no test compares it for equality:
//! the share is read from the note, which rounds it.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/stand_in.rs"]
mod stand_in;
#[allow(dead_code)] // this file uses a subset of the lifecycle rig
#[path = "support/agent_lifecycle.rs"]
mod support;
use support as lifecycle;
#[allow(dead_code)]
#[path = "support/message_delivery.rs"]
mod delivery_support;

use atm_core::SessionStatus;
use serde_json::{json, Value};
use stand_in::{StandInMode, StandInPane};
use support::{AgentRig, Started};

/// A delivery policy whose waits are as short as the policy allows: the
/// tests wait on events, and the defaults (ten seconds of idle) would only
/// make them long.
const DELIVERY: &str = "[delivery]\nstable_idle = { secs = 0, nanos = 1000000 }\nstable_empty_input = { secs = 0, nanos = 1000000 }\n";

/// The role's switch.
const ON: &str = "auto_handover = ['context']\n";

struct Seat {
    test: AgentRig,
    config: stand_in::StandInConfig,
    id: String,
    pane: StandInPane,
    /// The session the seat has before any handover.
    old: String,
    /// The audit cursor and the stand-in's event cursor at the start.
    cursor: u64,
    watch: std::cell::RefCell<delivery_support::Watch>,
    after: u64,
}

impl Seat {
    /// A ready agent `lead`, switched on, idle in a fresh session.
    fn start(mode: StandInMode) -> Self {
        let test = AgentRig::start_with_config(mode, DELIVERY);
        let config = support::started_config(mode);
        let started = Started::up_with(&test, "lead", &config, ON);
        let row = test.rig.wait_for_row(&started.pane, |r| {
            !r.id.is_pending() && r.status == SessionStatus::Idle
        });
        test.rig.wait_ready(&started.pane, Some(row.id.as_str()));
        let cursor = test.cursor();
        let watch = std::cell::RefCell::new(delivery_support::Watch::on(&test.rig, cursor));
        let after = test.rig.event_cursor(&started.pane);
        Self {
            config,
            id: started.id,
            pane: started.pane,
            old: row.id.as_str().to_owned(),
            cursor,
            watch,
            after,
            test,
        }
    }

    fn memory(&self) -> std::path::PathBuf {
        self.test.project.join(".atm/memory/lead.md")
    }

    fn control(&self, command: Value) -> Value {
        let reply = self.test.rig.control(&self.pane, command);
        assert!(reply["error"].is_null(), "{reply}");
        reply
    }

    fn advance(&self, by_ms: u64) {
        let now = self.control(json!({"command":"barrier"}))["now_ms"]
            .as_u64()
            .unwrap();
        self.control(json!({"command":"advance_to", "now_ms": now + by_ms}));
    }

    fn events(&self) -> Vec<Value> {
        self.test.tail(self.cursor)
    }

    fn count(&self, kind: &str) -> usize {
        self.events().iter().filter(|e| e["kind"] == kind).count()
    }

    /// A turn of the seat's own work starts, and its end will report
    /// `share` percent of the context.
    fn work_starts(&self, share: u32) {
        let after = self.test.rig.event_cursor(&self.pane);
        self.test.rig.paste(&self.pane, "the seat's own work");
        self.test.rig.wait_event(&self.pane, after, |e| {
            e["kind"] == "rendered" && e["state"] == "working" && e["draft"] == ""
        });
        self.test
            .rig
            .wait_for_row(&self.pane, |r| r.status == SessionStatus::Working);
        self.control(json!({"command":"set_context_share", "percent": share}));
    }

    /// The stand-in ends its turn; returns once its editor is idle again.
    fn work_ends(&self) {
        let before = self.test.rig.event_cursor(&self.pane);
        self.advance(self.config.work_ms);
        self.test.rig.wait_event(&self.pane, before, |e| {
            e["kind"] == "rendered" && e["state"] == "idle"
        });
    }

    /// With no command given, the stand-in takes the daemon's save request
    /// as a prompt and its turn runs. Returns the request's message id.
    fn save_turn_started(&self, after: u64) -> String {
        let accepted = self.test.rig.wait_event(&self.pane, after, |e| {
            e["kind"] == "accepted" && e["text"].as_str().is_some_and(|t| t.starts_with("[m"))
        });
        self.watch
            .borrow_mut()
            .wait("the delivery's record", |e| e["kind"] == "msg.delivered");
        assert_eq!(self.count("msg.delivered"), 1);
        let events = self.events();
        let requested = events
            .iter()
            .find(|e| e["kind"] == "agent.save_requested")
            .unwrap();
        assert_eq!(requested["body"]["agent_id"], self.id.as_str());
        let message = requested["body"]["message_id"].as_str().unwrap().to_owned();
        assert_eq!(
            accepted["text"],
            format!("[{message}] Read: atm inbox --id {message}").as_str(),
            "the request is typed as any message is"
        );
        let sent = events.iter().find(|e| e["kind"] == "msg.sent").unwrap();
        assert_eq!(sent["body"]["from"], "system");
        assert_eq!(sent["body"]["to"], self.id.as_str());
        assert_eq!(
            sent["body"]["body"],
            format!(
                "Handover: write what your next session needs to {}, then end your turn. \
                 Your session is cleared after that.",
                self.memory().display()
            )
            .as_str()
        );
        self.test
            .rig
            .wait_for_row(&self.pane, |r| r.status == SessionStatus::Working);
        message
    }

    /// Let the save turn end, then let the stand-in finish the clear the
    /// daemon types. Returns the new session's id.
    fn end_turn_and_clear(&self) -> String {
        let before_clear = self.test.rig.event_cursor(&self.pane);
        // Advance-then-wait is too late: completion can already trigger clear.
        self.test
            .finish_turn_at_ready_editor(&self.pane, || self.advance(self.config.work_ms));
        self.test.rig.wait_event(&self.pane, before_clear, |e| {
            e["kind"] == "rendered" && e["state"] == "clearing"
        });
        self.advance(self.config.clear_gap_ms);
        let row = self.test.rig.wait_for_row(&self.pane, |r| {
            !r.id.is_pending() && r.id.as_str() != self.old && r.status == SessionStatus::Idle
        });
        row.id.as_str().to_owned()
    }
}

/// A turn that ends above the line is all it takes: the save request, the
/// save, the clear, and one note to the seat's manager (the owner here).
fn handed_over_at_the_line(mode: StandInMode) {
    let mut seat = Seat::start(mode);
    seat.work_starts(63);
    let after = seat.test.rig.event_cursor(&seat.pane);
    seat.work_ends();
    let message = seat.save_turn_started(after);
    std::fs::create_dir_all(seat.memory().parent().unwrap()).unwrap();
    std::fs::write(seat.memory(), "what the next session needs").unwrap();
    let new = seat.end_turn_and_clear();
    assert_ne!(new, seat.old);
    seat.watch
        .borrow_mut()
        .wait("the agent bound in its new session", |e| {
            e["kind"] == "agent.session_bound"
                && e["body"]["agent_id"] == seat.id
                && e["body"]["session_id"] == new
        });
    let agent = seat.test.agent(&seat.id);
    assert_eq!(agent["state"], "ready");
    assert_eq!(agent["session_id"], new.as_str());
    seat.watch
        .borrow_mut()
        .wait("the note to the manager", |e| {
            e["kind"] == "msg.sent" && e["body"]["to"] == "owner"
        });
    assert_eq!(seat.count("msg.sent"), 2);
    let events = seat.events();
    let note = events.iter().rfind(|e| e["kind"] == "msg.sent").unwrap();
    assert_eq!(note["body"]["from"], "system");
    assert_eq!(note["body"]["to"], "owner");
    assert_eq!(
        note["body"]["body"],
        format!(
            "automatic handover of {} done (context 63%, line 60%): it saved to {} ({message}) \
             and has a new session.",
            seat.id,
            seat.memory().display()
        )
        .as_str()
    );
    // One handover, and no note at the line beside it.
    assert_eq!(seat.count("agent.save_requested"), 1);
    assert_eq!(seat.count("agent.saved"), 1);
    assert_eq!(seat.count("agent.clear_requested"), 1);
    // Two prompts since the start: the seat's own work and the save request.
    let journal = seat.test.rig.journal(&seat.pane);
    let prompts: Vec<_> = journal
        .iter()
        .filter(|e| e["event_sequence"].as_u64().unwrap() > seat.after)
        .filter(|e| matches!(e["kind"].as_str(), Some("accepted" | "rejected" | "queued")))
        .collect();
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    seat.test.rig.close();
}

#[test]
fn a_seat_at_its_line_is_handed_over_with_no_command_pi() {
    handed_over_at_the_line(StandInMode::Pi);
}

#[test]
fn a_seat_at_its_line_is_handed_over_with_no_command_claude() {
    handed_over_at_the_line(StandInMode::Claude);
}

/// The editor looks idle but the harness has not reported the turn's end:
/// the daemon asks for nothing until it has.
#[test]
fn nothing_is_asked_before_the_turn_end_is_reported_pi() {
    let mut seat = Seat::start(StandInMode::Pi);
    seat.work_starts(63);
    seat.control(json!({"command":"hold_next_hook"}));
    let after = seat.test.rig.event_cursor(&seat.pane);
    seat.work_ends();
    seat.test
        .rig
        .wait_event(&seat.pane, after, |e| e["kind"] == "held");
    // The stand-in's editor is idle and empty. The daemon still holds the
    // row as working, and has queued no request.
    seat.control(json!({"command":"barrier"}));
    let row = seat
        .test
        .rig
        .wait_for_row(&seat.pane, |r| r.id.as_str() == seat.old);
    assert_eq!(row.status, SessionStatus::Working);
    assert_eq!(seat.count("agent.save_requested"), 0);
    assert_eq!(seat.count("msg.sent"), 0);
    // The end is reported: now the request comes.
    seat.control(json!({"command":"release_hook"}));
    seat.save_turn_started(after);
    assert_eq!(seat.count("agent.save_requested"), 1);
    seat.test.rig.close();
}

/// The harness does not take the pasted request: nothing is saved and
/// nothing is cleared.
#[test]
fn a_request_the_harness_does_not_take_clears_nothing_pi() {
    let mut seat = Seat::start(StandInMode::Pi);
    seat.work_starts(63);
    seat.control(json!({"command":"refuse_next_paste"}));
    let after = seat.test.rig.event_cursor(&seat.pane);
    seat.work_ends();
    let rejected = seat.test.rig.wait_event(&seat.pane, after, |e| {
        e["kind"] == "rejected" && e["reason"] == "refuse_next_paste"
    });
    seat.watch.borrow_mut().wait("the request's record", |e| {
        e["kind"] == "agent.save_requested"
    });
    assert_eq!(seat.count("agent.save_requested"), 1);
    let events = seat.events();
    let requested = events
        .iter()
        .find(|e| e["kind"] == "agent.save_requested")
        .unwrap();
    let message = requested["body"]["message_id"].as_str().unwrap();
    assert_eq!(
        rejected["text"],
        format!("[{message}] Read: atm inbox --id {message}").as_str()
    );
    seat.control(json!({"command":"barrier"}));
    assert_eq!(seat.count("msg.delivered"), 0);
    assert_eq!(seat.count("agent.saved"), 0);
    assert_eq!(seat.count("agent.clear_requested"), 0);
    let agent = seat.test.agent(&seat.id);
    assert_eq!(agent["state"], "ready", "{agent}");
    assert_eq!(agent["session_id"], seat.old.as_str(), "{agent}");
    seat.test.rig.close();
}
