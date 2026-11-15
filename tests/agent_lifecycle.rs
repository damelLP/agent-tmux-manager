//! Real-daemon agent lifecycle acceptance using the shared, model-free stand-in.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/stand_in.rs"]
mod stand_in;
#[path = "support/agent_lifecycle.rs"]
mod support;

use atm_core::SessionStatus;
use serde_json::json;
use stand_in::StandInMode;
use support::{checked, process_identity, AgentRig};

/// Adoption must bind the existing process/session, not launch or type into it.
#[test]
fn adopt_keeps_pid_and_existing_session() {
    let mut test = AgentRig::start(StandInMode::Claude);
    let pane = test.shell_agent("lead", &support::config(StandInMode::Claude));
    let row = test.complete_fixture_turn(&pane);
    let identity = process_identity(pane.pid);
    let screen = test.rig.capture_styled(&pane);
    let after = test.rig.event_cursor(&pane);
    let audit = test.cursor();
    let result = test.adopt("lead", &pane);
    assert_eq!(result["state"], "ready", "{result}");
    let id = result["agent_id"].as_str().unwrap();
    let agent = test.agent(id);
    assert_eq!(agent["state"], "ready", "{agent}");
    assert_eq!(agent["pane"], pane.pane_id);
    assert_eq!(agent["session_id"], row.id.as_str());
    assert_eq!(
        checked(test.rig.tmux(&[
            "show-options",
            "-p",
            "-v",
            "-t",
            &pane.pane_id,
            "@atm-agent"
        ])),
        id
    );
    test.assert_no_input_since(&pane, after);
    assert_eq!(process_identity(pane.pid), identity);
    assert_eq!(test.rig.capture_styled(&pane), screen);
    assert_eq!(test.rig.pane_rows(&pane).len(), 1);
    assert_eq!(test.rig.pane_rows(&pane)[0].id, row.id);
    let events = test.tail(audit);
    assert!(events
        .iter()
        .any(|e| e["kind"] == "agent.adopted" && e["body"]["agent_id"] == id));
    assert!(
        !events
            .iter()
            .any(|e| e["kind"].as_str().unwrap().starts_with("control.")),
        "{events:?}"
    );
    // A repeated adoption must keep the same binding, too.
    assert_eq!(test.adopt("lead", &pane), result);
    assert_eq!(test.agent(id)["session_id"], row.id.as_str());
    test.assert_no_input_since(&pane, after);
    test.rig.close();
}

/// Killing must remove exactly the owned pane and durably audit before Down.
#[test]
fn down_kill_removes_pane_and_records_audit() {
    let mut test = AgentRig::start(StandInMode::Claude);
    let pane = test.shell_agent("lead", &support::config(StandInMode::Claude));
    test.complete_fixture_turn(&pane);
    let other = test.shell_agent("other", &support::config(StandInMode::Claude));
    let other_row = test.complete_fixture_turn(&other);
    let other_identity = process_identity(other.pid);
    let other_screen = test.rig.capture_styled(&other);
    let other_cursor = test.rig.event_cursor(&other);
    let adopted = test.adopt("lead", &pane);
    assert_eq!(adopted["state"], "ready");
    let id = adopted["agent_id"].as_str().unwrap();
    let cursor = test.cursor();
    let before_kill = test.rig.event_cursor(&pane);
    let result = test.rpc("agent.down", json!({"agent": id, "kill": true}), None);
    assert_eq!(result, json!({"agent_id": id}));
    stand_in::process::wait_exit(pane.pid, std::time::Duration::from_secs(30)).unwrap();
    // A killed process needs no ending hook or editor input to complete Down.
    assert!(test
        .rig
        .journal(&pane)
        .iter()
        .all(|e| e["event_sequence"].as_u64().unwrap() <= before_kill));
    let panes = checked(test.rig.tmux(&["list-panes", "-a", "-F", "#{pane_id}"]));
    assert!(!panes.lines().any(|p| p == pane.pane_id));
    assert!(panes.lines().any(|p| p == other.pane_id));
    let agent = test.agent(id);
    assert_eq!(agent["state"], "down");
    assert_eq!(agent["pane"], json!(null));
    assert_eq!(agent["session_id"], json!(null));
    assert_eq!(
        agent["reason"],
        format!("taken down; {} killed", pane.pane_id)
    );
    let events = test.tail(cursor);
    let kills: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "control.killed")
        .collect();
    assert_eq!(kills.len(), 1, "{events:?}");
    assert_eq!(
        kills[0]["body"],
        json!({"pane": pane.pane_id, "action": {"method":"agent.down", "agent":id}})
    );
    let downs: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "agent.down" && e["body"]["agent_id"] == id)
        .collect();
    assert_eq!(downs.len(), 1, "{events:?}");
    assert!(kills[0]["seq"].as_u64().unwrap() < downs[0]["seq"].as_u64().unwrap());
    let cursor = test.cursor();
    test.rpc("agent.down", json!({"agent":id, "kill":true}), None);
    assert!(!test
        .tail(cursor)
        .iter()
        .any(|e| e["kind"] == "control.killed" || e["kind"] == "agent.down"));
    assert_eq!(process_identity(other.pid), other_identity);
    assert_eq!(test.rig.capture_styled(&other), other_screen);
    assert_eq!(test.rig.pane_rows(&other)[0].id, other_row.id);
    test.assert_no_input_since(&other, other_cursor);
    test.rig.close();
}

/// Release must untag, not kill or send an exit command to the existing harness.
#[test]
fn down_release_keeps_process_and_removes_tag() {
    let mut test = AgentRig::start(StandInMode::Claude);
    let pane = test.shell_agent("lead", &support::config(StandInMode::Claude));
    let row = test.complete_fixture_turn(&pane);
    let adopted = test.adopt("lead", &pane);
    assert_eq!(adopted["state"], "ready");
    let id = adopted["agent_id"].as_str().unwrap();
    let identity = process_identity(pane.pid);
    let screen = test.rig.capture_styled(&pane);
    let after = test.rig.event_cursor(&pane);
    let cursor = test.cursor();
    test.rpc("agent.down", json!({"agent":id, "kill":false}), None);
    let agent = test.agent(id);
    assert_eq!(agent["state"], "down");
    assert_eq!(agent["pane"], json!(null));
    assert_eq!(agent["session_id"], json!(null));
    assert_eq!(
        agent["reason"],
        format!(
            "taken down; {} released with its program running",
            pane.pane_id
        )
    );
    let options = checked(test.rig.tmux(&["show-options", "-p", "-t", &pane.pane_id]));
    assert!(
        !options.lines().any(|line| line.starts_with("@atm-agent ")),
        "{options}"
    );
    assert_eq!(process_identity(pane.pid), identity);
    assert_eq!(test.rig.capture_styled(&pane), screen);
    assert_eq!(test.rig.pane_rows(&pane)[0].id, row.id);
    test.assert_no_input_since(&pane, after);
    let events = test.tail(cursor);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["kind"] == "agent.down" && e["body"]["agent_id"] == id)
            .count(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|e| e["kind"].as_str().unwrap().starts_with("control.")),
        "{events:?}"
    );
    test.rig.close();
}

/// A safe editor is insufficient when a noninteractive shell holds its child.
#[test]
fn adopt_noninteractive_launcher_stays_starting() {
    let mut test = AgentRig::start(StandInMode::Claude);
    let pane = test.rig.launch(&support::config(StandInMode::Claude));
    test.complete_fixture_turn(&pane);
    let identity = process_identity(pane.pid);
    let after = test.rig.event_cursor(&pane);
    let cursor = test.cursor();
    let result = test.adopt("lead", &pane);
    assert_eq!(result["state"], "starting", "{result}");
    // Inspect the adoption's own committed event. The old observer has not yet
    // acquired Task 10's foreground verdict; its later state is not this call.
    let events = test.tail(cursor);
    let adopted = events
        .iter()
        .find(|e| e["kind"] == "agent.adopted")
        .unwrap();
    assert_eq!(adopted["body"]["session_id"], json!(null));
    assert_eq!(process_identity(pane.pid), identity);
    test.assert_no_input_since(&pane, after);
    test.rig.close();
}

/// Private selectors/env must not reach the live server or installed harnesses.
#[test]
fn lifecycle_rig_is_private_and_daemon_override_is_set() {
    let mut first = AgentRig::start(StandInMode::Claude);
    let mut second = AgentRig::start(StandInMode::Pi);
    assert_ne!(first.rig.root(), second.rig.root());
    assert_ne!(first.rig.socket(), second.rig.socket());
    assert_ne!(first.server(), second.server());
    for (test, mode) in [(&first, StandInMode::Claude), (&second, StandInMode::Pi)] {
        let pids = test.rig.daemon_pids();
        assert_eq!(pids.len(), 1);
        let env = std::fs::read(format!("/proc/{}/environ", pids[0])).unwrap();
        let entries: Vec<_> = env.split(|b| *b == 0).collect();
        for (key, path) in [
            ("ATM_SOCKET", test.rig.socket()),
            ("ATM_STATE_DIR", test.rig.root().join("state")),
            ("TMUX_TMPDIR", test.rig.root().join("tmux")),
            ("TMPDIR", test.rig.root().join("tmp")),
            ("HOME", test.rig.root().join("home")),
            ("ATM_SPAWN_BIN", test.rig.launcher(mode)),
        ] {
            assert!(
                entries.contains(&format!("{key}={}", path.display()).as_bytes()),
                "missing private {key}"
            );
        }
        assert!(!entries
            .iter()
            .any(|e| e.starts_with(b"TMUX=") || e.starts_with(b"TMUX_PANE=")));
        assert_eq!(
            checked(
                test.rig
                    .tmux(&["show-options", "-g", "-v", "default-shell"])
            ),
            "/bin/bash"
        );
        assert_eq!(
            checked(
                test.rig
                    .tmux(&["show-options", "-g", "-v", "default-command"])
            ),
            "/bin/bash --noprofile --norc"
        );
        assert!(checked(test.rig.tmux(&["list-clients"])).is_empty());
    }
    let a = first.shell_agent("first", &support::config(StandInMode::Claude));
    let b = second.shell_agent("second", &support::config(StandInMode::Pi));
    for (test, pane) in [(&first, &a), (&second, &b)] {
        assert_eq!(
            checked(test.rig.tmux(&[
                "display-message",
                "-p",
                "-t",
                &pane.pane_id,
                "#{pane_width} #{pane_height} #{session_attached}"
            ])),
            "200 50 0"
        );
        test.rig.wait_for_row(pane, |r| {
            !r.id.is_pending() && r.status == SessionStatus::Idle
        });
    }
    let identity = process_identity(b.pid);
    first.rig.close();
    assert_eq!(process_identity(b.pid), identity);
    assert!(!second.rig.pane_rows(&b).is_empty());
    second.rig.close();
}
