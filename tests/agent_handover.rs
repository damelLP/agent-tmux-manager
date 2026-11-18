//! Real-daemon handover acceptance on the shared, model-free stand-in: the
//! save request travels through the message runtime into the stand-in's
//! editor, the test plays the agent (it writes the memory file and lets the
//! turn end), and the daemon clears the session with the real `agent.clear`.
//! Every wait is on a stand-in event, a registry row or a process exit.
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

/// The longest save the method accepts. No test here is about the save's
/// deadline, so none may race it on a loaded machine: the passing paths never
/// wait for it, and the rig's event waits are the hang guards.
const SAVE_TIMEOUT_MS: &str = "1800000";

fn save_timeout_ms() -> u64 {
    SAVE_TIMEOUT_MS.parse().unwrap()
}

struct Handover {
    test: AgentRig,
    config: stand_in::StandInConfig,
    id: String,
    pane: StandInPane,
    /// The session the agent has before the handover.
    old: String,
    /// The audit cursor and the stand-in's event cursor before the handover.
    cursor: u64,
    watch: std::cell::RefCell<delivery_support::Watch>,
    after: u64,
}

impl Handover {
    /// A ready agent `lead` with one finished turn behind it.
    fn start(mode: StandInMode) -> Self {
        let test = AgentRig::start_with_config(mode, DELIVERY);
        let config = support::started_config(mode);
        let started = Started::up(&test, "lead", &config);
        let old = test.complete_fixture_turn(&started.pane);
        let cursor = test.cursor();
        let watch = std::cell::RefCell::new(delivery_support::Watch::on(&test.rig, cursor));
        let after = test.rig.event_cursor(&started.pane);
        Self {
            config,
            id: started.id,
            pane: started.pane,
            old: old.id.as_str().to_owned(),
            cursor,
            watch,
            after,
            test,
        }
    }

    fn memory(&self) -> std::path::PathBuf {
        self.test.project.join(".atm/memory/lead.md")
    }

    fn events(&self) -> Vec<Value> {
        self.test.tail(self.cursor)
    }

    fn kinds(&self) -> Vec<String> {
        self.events()
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_owned())
            .collect()
    }

    fn advance(&self, by_ms: u64) {
        let now = self
            .test
            .rig
            .control(&self.pane, json!({"command":"barrier"}))["now_ms"]
            .as_u64()
            .unwrap();
        let reply = self.test.rig.control(
            &self.pane,
            json!({"command":"advance_to", "now_ms": now + by_ms}),
        );
        assert!(reply["error"].is_null(), "{reply}");
    }

    /// The stand-in takes the save request as a prompt and its turn runs.
    /// Returns the request's message id.
    fn save_turn_started(&self) -> String {
        let accepted = self
            .test
            .rig
            .wait_event(&self.pane, self.after, |e| e["kind"] == "accepted");
        self.watch
            .borrow_mut()
            .wait("the delivery's record", |e| e["kind"] == "msg.delivered");
        assert!(self.kinds().iter().any(|kind| kind == "msg.delivered"));
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

    /// The agent's part: write the memory file.
    fn save(&self) {
        std::fs::create_dir_all(self.memory().parent().unwrap()).unwrap();
        std::fs::write(self.memory(), "what the next session needs").unwrap();
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

    /// One prompt since the handover began: the save request. The clear
    /// command was not a prompt and nothing was joined to it.
    fn assert_one_prompt(&self) {
        let journal = self.test.rig.journal(&self.pane);
        let prompts: Vec<_> = journal
            .iter()
            .filter(|e| e["event_sequence"].as_u64().unwrap() > self.after)
            .filter(|e| matches!(e["kind"].as_str(), Some("accepted" | "rejected" | "queued")))
            .collect();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
    }

    /// The handover's own records, in the order they were committed.
    fn assert_recorded(&self, expected: &[&str]) {
        let kinds = self.kinds();
        let found: Vec<&str> = kinds
            .iter()
            .map(String::as_str)
            .filter(|kind| expected.contains(kind))
            .collect();
        assert_eq!(found, expected, "{kinds:?}");
    }
}

const WHOLE_HANDOVER: &[&str] = &[
    "msg.sent",
    "agent.save_requested",
    "msg.delivery_attempted",
    "msg.delivered",
    "agent.saved",
    "agent.clear_requested",
    "agent.session_bound",
];

/// No handover runs here: pin the fixture's completion/frame boundary without
/// relying on whether a clear happens to win the terminal race.
fn save_turn_completion_is_reported_after_idle_render(mode: StandInMode) {
    let mut test = AgentRig::start_with_config(mode, DELIVERY);
    let config = support::started_config(mode);
    let started = Started::up(&test, "lead", &config);
    let original = test.rig.wait_for_row(&started.pane, |r| {
        !r.id.is_pending() && r.status == SessionStatus::Idle
    });
    test.rig
        .wait_ready(&started.pane, Some(original.id.as_str()));
    let before_work = test.rig.event_cursor(&started.pane);
    test.rig.paste(&started.pane, "fixture save turn");
    test.rig.wait_event(&started.pane, before_work, |e| {
        e["kind"] == "rendered" && e["state"] == "working" && e["draft"] == ""
    });
    test.rig
        .wait_for_row(&started.pane, |r| r.status == SessionStatus::Working);
    let now = test
        .rig
        .control(&started.pane, json!({"command":"barrier"}))["now_ms"]
        .as_u64()
        .unwrap();
    let before_finish = test.rig.event_cursor(&started.pane);
    let (pid, start_time) = support::process_identity(started.pane.pid);
    let cursor = test.cursor();
    let completion: &[&str] = match mode {
        StandInMode::Pi => &["turn_end", "agent_end"],
        StandInMode::Claude | StandInMode::Codex => &["Stop"],
    };
    let events = || {
        test.rig
            .journal(&started.pane)
            .into_iter()
            .filter(|e| {
                e["pid"] == pid
                    && e["start_time"] == start_time
                    && e["event_sequence"].as_u64().unwrap() > before_finish
            })
            .collect::<Vec<_>>()
    };
    test.finish_turn_at_ready_editor(&started.pane, || {
        test.rig.control(
            &started.pane,
            json!({"command":"advance_to", "now_ms":now + config.work_ms}),
        );
        // The advance acknowledgement orders all completion effects, but not
        // tmux's acknowledgement of the idle frame. Do not wait for it here:
        // that fence belongs to the helper, after this callback returns.
        let journal = events();
        let held: Vec<_> = journal
            .iter()
            .filter(|e| e["kind"] == "held")
            .map(|e| e["event"].as_str().unwrap())
            .collect();
        assert_eq!(held, completion, "completion must be held before advance");
        assert!(journal.iter().all(|e| e["kind"] != "event"), "{journal:?}");
        let row = test
            .rig
            .wait_for_row(&started.pane, |r| r.id == original.id);
        assert_eq!(row.status, SessionStatus::Working);
        assert!(test.tail(cursor).iter().all(|e| {
            !matches!(
                e["kind"].as_str(),
                Some("agent.saved" | "agent.clear_requested")
            )
        }));
    });
    let journal = events();
    let idle = journal
        .iter()
        .find(|e| e["kind"] == "rendered" && e["state"] == "idle" && e["draft"] == "")
        .expect("the idle frame must be terminal-acknowledged before release");
    let idle_sequence = idle["event_sequence"].as_u64().unwrap();
    let released: Vec<_> = journal.iter().filter(|e| e["kind"] == "event").collect();
    assert_eq!(
        released
            .iter()
            .map(|e| e["event"].as_str().unwrap())
            .collect::<Vec<_>>(),
        completion,
        "each completion event must be released exactly once"
    );
    for held in journal.iter().filter(|e| e["kind"] == "held") {
        assert!(held["event_sequence"].as_u64().unwrap() < idle_sequence);
    }
    for event in released {
        assert!(idle_sequence < event["event_sequence"].as_u64().unwrap());
    }
    let row = test
        .rig
        .wait_for_row(&started.pane, |r| r.status == SessionStatus::Idle);
    assert_eq!(row.id, original.id);
    test.rig.close();
}

#[test]
fn save_turn_completion_is_reported_after_idle_render_claude() {
    save_turn_completion_is_reported_after_idle_render(StandInMode::Claude);
}

#[test]
fn save_turn_completion_is_reported_after_idle_render_pi() {
    save_turn_completion_is_reported_after_idle_render(StandInMode::Pi);
}

fn spawn_handover_cli(h: &Handover) -> (u32, stand_in::process::OwnedChild) {
    let mut command = h
        .test
        .rig
        .command(std::path::Path::new(env!("CARGO_BIN_EXE_atm")));
    let handover = command
        .args([
            "agent",
            "handover",
            &h.id,
            "--timeout-ms",
            SAVE_TIMEOUT_MS,
            "--format",
            "json",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    (handover.id(), stand_in::process::OwnedChild::new(handover))
}

#[test]
fn handover_cli_is_reaped_when_idle_frame_fence_unwinds_pi() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let mut h = Handover::start(StandInMode::Pi);
    let (pid, handover) = spawn_handover_cli(&h);
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(raw >= 0);
    let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _handover = handover;
        h.save_turn_started();
        h.save();
        h.test.finish_turn_at_ready_editor(&h.pane, || {
            h.advance(h.config.work_ms);
            panic!("injected idle-frame fence failure");
        });
    }));
    // Inspect completion before daemon teardown or fallback cleanup can mask
    // a missing CLI guard. The pidfd cannot refer to a reused PID.
    let mut status = unsafe { std::mem::zeroed() };
    let observed = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            fd.as_raw_fd() as u32,
            &mut status,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    let error = std::io::Error::last_os_error().raw_os_error();
    let reaped = observed == -1 && error == Some(libc::ECHILD);
    eprintln!("CLI {pid}: before fallback waitid={observed}, errno={error:?}");
    if !reaped {
        // Clean a failing raw-child regression too, before asserting: it is
        // still our unreaped child, pinned by this exact pidfd.
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    libc::SIGKILL,
                    0,
                    0,
                )
            },
            0
        );
        let mut event = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(unsafe { libc::poll(&mut event, 1, 30_000) } > 0);
        assert_eq!(
            unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    fd.as_raw_fd() as u32,
                    &mut status,
                    libc::WEXITED,
                )
            },
            0
        );
        eprintln!("CLI {pid}: fallback killed and reaped the owned child");
    }
    h.test.rig.close();
    assert_eq!(
        result.unwrap_err().downcast_ref::<&str>(),
        Some(&"injected idle-frame fence failure")
    );
    assert!(
        reaped,
        "CLI guard did not reap on fence unwind: waitid={observed}, errno={error:?}"
    );
}

/// The `atm` binary: `atm agent handover` waits for the save and the clear
/// and prints the daemon's answer.
#[test]
fn atm_agent_handover_saves_then_clears_pi() {
    let mut h = Handover::start(StandInMode::Pi);
    let (_, handover) = spawn_handover_cli(&h);
    let message = h.save_turn_started();
    h.save();
    let new = h.end_turn_and_clear();
    let output = handover.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let answer: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        answer,
        json!({
            "agent_id": h.id,
            "message_id": message,
            "state": "cleared",
            "session_id": new,
        })
    );
    assert_ne!(new, h.old);
    let agent = h.test.agent(&h.id);
    assert_eq!(agent["state"], "ready", "{agent}");
    assert_eq!(agent["session_id"], new.as_str(), "{agent}");
    h.assert_recorded(WHOLE_HANDOVER);
    h.assert_one_prompt();
    h.test.rig.close();
}

/// A save turn that ends with the memory file as it was: refused with the
/// reason, and the session is not cleared.
#[test]
fn a_turn_that_does_not_save_is_not_cleared_pi() {
    let mut h = Handover::start(StandInMode::Pi);
    let call = h.test.call_in_background(
        "agent.handover",
        json!({"agent": h.id, "timeout_ms": save_timeout_ms()}),
    );
    h.save_turn_started();
    let before_idle = h.test.rig.event_cursor(&h.pane);
    h.advance(h.config.work_ms);
    h.test.rig.wait_event(&h.pane, before_idle, |e| {
        e["kind"] == "rendered" && e["state"] == "idle"
    });
    let error = call.join().unwrap().unwrap_err();
    assert!(error.contains("refused"), "{error}");
    assert!(
        error.contains(&format!(
            "{} ended its save turn and {} did not change; nothing was cleared",
            h.id,
            h.memory().display()
        )),
        "{error}"
    );
    let agent = h.test.agent(&h.id);
    assert_eq!(agent["state"], "ready", "{agent}");
    assert_eq!(agent["session_id"], h.old.as_str(), "{agent}");
    h.assert_recorded(&[
        "msg.sent",
        "agent.save_requested",
        "msg.delivery_attempted",
        "msg.delivered",
    ]);
    h.assert_one_prompt();
    h.test.rig.close();
}

/// An agent asks for its own handover: the call from its pane answers at
/// once, and the handover carries on to the clear.
#[test]
fn an_agent_hands_itself_over_claude() {
    let mut h = Handover::start(StandInMode::Claude);
    let answer = h.test.rpc(
        "agent.handover",
        json!({"agent": h.id, "timeout_ms": save_timeout_ms()}),
        Some(h.pane.pane_id.clone()),
    );
    assert_eq!(answer["state"], "started", "{answer}");
    assert_eq!(answer["session_id"], Value::Null, "{answer}");
    let message = h.save_turn_started();
    assert_eq!(answer["message_id"], message.as_str());
    h.save();
    let new = h.end_turn_and_clear();
    h.watch
        .borrow_mut()
        .wait("the agent bound in its new session", |e| {
            e["kind"] == "agent.session_bound"
                && e["body"]["agent_id"] == h.id
                && e["body"]["session_id"] == new
        });
    let agent = h.test.agent(&h.id);
    assert_eq!(agent["state"], "ready");
    assert_eq!(agent["session_id"], new.as_str());
    h.assert_recorded(WHOLE_HANDOVER);
    h.assert_one_prompt();
    h.test.rig.close();
}
