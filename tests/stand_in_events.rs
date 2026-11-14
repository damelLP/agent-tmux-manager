//! Runtime synchronization contract: no journal polling or readiness delays.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/stand_in.rs"]
mod stand_in;

use serde_json::json;
use stand_in::{StandInConfig, StandInMode, StandInRig};

#[test]
fn manual_time_holds_real_pane_work_and_clear() {
    let mut rig = StandInRig::start();
    let mut config = StandInConfig::new(StandInMode::Claude);
    config.manual_clock = true;
    let pane = rig.launch(&config);
    let resized = rig.tmux(&[
        "resize-window",
        "-t",
        &pane.pane_id,
        "-x",
        "120",
        "-y",
        "40",
    ]);
    assert!(resized.status.success(), "{resized:?}");
    let initial = rig.event_cursor(&pane);
    rig.paste(&pane, "held turn");
    let working = rig.wait_event(&pane, initial, |e| {
        e["kind"] == "rendered" && e["state"] == "working"
    });
    let reply = rig.control(&pane, json!({"command": "advance_to", "now_ms": 499}));
    assert_eq!(reply["state"], "working");
    let capture = rig.capture_plain(&pane);
    assert!(capture.contains(": working"), "{capture:?}");
    rig.control(&pane, json!({"command": "advance_to", "now_ms": 500}));
    let idle = rig.wait_event(&pane, working["event_sequence"].as_u64().unwrap(), |e| {
        e["kind"] == "rendered" && e["state"] == "idle"
    });
    let after = idle["event_sequence"].as_u64().unwrap();
    rig.paste(&pane, "/clear");
    let clear = rig.wait_event(&pane, after, |e| {
        e["kind"] == "rendered" && e["state"] == "clearing"
    });
    rig.control(&pane, json!({"command": "advance_to", "now_ms": 599}));
    assert!(rig.capture_plain(&pane).contains(": clearing"));
    rig.control(&pane, json!({"command": "advance_to", "now_ms": 600}));
    let fresh = rig.wait_event(&pane, clear["event_sequence"].as_u64().unwrap(), |e| {
        e["kind"] == "rendered" && e["state"] == "idle"
    });
    assert_ne!(fresh["session_id"], idle["session_id"]);
    let output = rig.capture_plain(&pane);
    assert!(output.starts_with("stand-in claude "));
    assert!(
        output.ends_with('\n'),
        "capture must retain viewport line structure"
    );
    rig.close();
}

#[test]
fn event_replay_is_scoped_to_restarted_process_not_reused_pane_or_log() {
    let mut rig = StandInRig::start();
    let config = StandInConfig::new(StandInMode::Codex);
    let first = rig.launch(&config);
    let old = rig.wait_event(&first, 0, |e| e["kind"] == "ready");
    rig.paste(&first, "/quit");
    rig.wait_for_shell(&first);
    let output = rig.tmux(&["respawn-pane", "-k", "-t", &first.pane_id]);
    assert!(output.status.success(), "{output:?}");
    let second = rig.observe_pane(first.pane_id.clone(), StandInMode::Codex, first.log.clone());
    let new = rig.wait_event(&second, 0, |e| e["kind"] == "ready");
    assert_eq!(new["pid"], second.pid);
    assert_ne!(old["pid"], new["pid"]);
    assert_ne!(old["session_id"], new["session_id"]);
    assert_eq!(old["pane"], new["pane"]);
    assert_eq!(new["sequence"], 1, "frame sequence restarts per process");
    rig.close();
}
