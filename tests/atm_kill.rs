//! `atm kill` through a real private daemon and a real private tmux server,
//! on the shared model-free stand-in. The kill is a daemon request: the pane
//! closes, the action is logged, and a named agent that lived in the pane is
//! recorded as down.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/stand_in.rs"]
mod stand_in;
#[allow(dead_code)] // this file uses a subset of the lifecycle rig
#[path = "support/agent_lifecycle.rs"]
mod support;

use std::time::Duration;

use serde_json::Value;
use stand_in::StandInMode;
use support::{checked, AgentRig};

fn panes(test: &AgentRig) -> Vec<String> {
    checked(test.rig.tmux(&["list-panes", "-a", "-F", "#{pane_id}"]))
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The log after `cursor`, once `found` is in it. An audit is written after
/// the reply and the log has no subscription here: a bounded condition poll.
fn log_with(test: &AgentRig, cursor: u64, found: impl Fn(&Value) -> bool) -> Vec<Value> {
    let mut log = Vec::new();
    test.rig.wait("the expected event in the log", || {
        log = test.tail(cursor);
        log.iter().any(&found)
    });
    log
}

fn killed(pane: &str) -> impl Fn(&Value) -> bool + '_ {
    move |event| event["kind"] == "control.killed" && event["body"]["pane"] == pane
}

fn millis(event: &Value) -> i64 {
    chrono::DateTime::parse_from_rfc3339(event["ts"].as_str().unwrap())
        .unwrap()
        .timestamp_millis()
}

#[test]
fn kill_closes_the_sessions_pane_and_is_logged() {
    let mut test = AgentRig::start(StandInMode::Claude);
    let config = support::config(StandInMode::Claude);
    let pane = test.shell_agent("victim", &config);
    let row = test.complete_fixture_turn(&pane);
    // A second pane: the private tmux server outlives the kill.
    let other = test.shell_agent("other", &config);
    let other_row = test.complete_fixture_turn(&other);
    let cursor = test.cursor();

    let said = checked(test.rig.atm(&["kill", row.id.as_str()]));
    assert_eq!(said, format!("Killed {}", pane.pane_id));
    stand_in::process::wait_exit(pane.pid, Duration::from_secs(30)).unwrap();
    test.rig.wait_for_absent(&row.id);
    let left = panes(&test);
    assert!(!left.contains(&pane.pane_id), "{left:?}");
    assert!(left.contains(&other.pane_id), "{left:?}");
    assert_eq!(test.rig.pane_rows(&other)[0].id, other_row.id);

    let log = log_with(&test, cursor, killed(&pane.pane_id));
    let control: Vec<_> = log
        .iter()
        .filter(|event| event["kind"].as_str().unwrap().starts_with("control."))
        .collect();
    assert_eq!(control.len(), 1, "{control:?}");
    let action = &control[0]["body"]["action"];
    assert_eq!(action["method"], "session.kill");
    assert_eq!(action["outcome"], "ok");
    assert_eq!(action["client_id"], "atm-rpc");
    test.rig.close();
}

#[test]
fn killing_a_named_agents_pane_records_the_agent_down() {
    let mut test = AgentRig::start(StandInMode::Claude);
    let config = support::config(StandInMode::Claude);
    let pane = test.shell_agent("lead", &config);
    test.complete_fixture_turn(&pane);
    let other = test.shell_agent("other", &config);
    test.complete_fixture_turn(&other);
    let adopted = test.adopt("lead", &pane);
    assert_eq!(adopted["state"], "ready", "{adopted}");
    let id = adopted["agent_id"].as_str().unwrap();
    let cursor = test.cursor();

    let said = checked(test.rig.atm(&["kill", &pane.pane_id]));
    assert_eq!(said, format!("Killed {}", pane.pane_id));
    let log = log_with(&test, cursor, |event| {
        event["kind"] == "agent.down" && event["body"]["agent_id"] == id
    });
    let down = log.iter().find(|e| e["kind"] == "agent.down").unwrap();
    assert_eq!(
        down["body"]["reason"],
        "recorded pane is absent, dead or no longer owned"
    );
    let kill = log.iter().find(|e| killed(&pane.pane_id)(e)).unwrap();
    assert_eq!(kill["body"]["action"]["method"], "session.kill");
    assert!(kill["seq"].as_u64() < down["seq"].as_u64());
    let agent = test.agent(id);
    assert_eq!(agent["state"], "down", "{agent}");
    assert!(!panes(&test).contains(&pane.pane_id));
    // The measure of how soon the record follows the kill; shown, not asserted.
    eprintln!(
        "kill logged -> agent.down logged: {} ms",
        millis(down) - millis(kill)
    );
    test.rig.close();
}
