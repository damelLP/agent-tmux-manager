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
    assert!(capture.contains("Levitating…"), "{capture:?}");
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
    assert!(rig.capture_plain(&pane).contains("running SessionEnd hook"));
    rig.control(&pane, json!({"command": "advance_to", "now_ms": 600}));
    let fresh = rig.wait_event(&pane, clear["event_sequence"].as_u64().unwrap(), |e| {
        e["kind"] == "rendered" && e["state"] == "idle"
    });
    assert_ne!(fresh["session_id"], idle["session_id"]);
    let output = rig.capture_plain(&pane);
    assert!(output.contains("Claude Code v2.1.325"));
    assert!(
        output.ends_with('\n'),
        "capture must retain viewport line structure"
    );
    rig.close();
}

/// Synthetic non-acceptance after tmux pasted bytes, not a pre-send guard refusal.
#[test]
fn refused_paste_is_one_shot_and_never_submitted() {
    let mut rig = StandInRig::start();
    for mode in [StandInMode::Claude, StandInMode::Codex, StandInMode::Pi] {
        let mut config = StandInConfig::new(mode);
        config.manual_clock = true;
        let pane = rig.launch(&config);
        let cursor = rig.event_cursor(&pane);
        let armed = rig.control(&pane, json!({"command": "refuse_next_paste"}));
        assert_eq!(armed["state"], "idle");
        let text = "/atm/dialog\nλ not submitted;";
        rig.paste(&pane, text);
        rig.wait_event(&pane, cursor, |e| {
            e["kind"] == "rejected" && e["reason"] == "refuse_next_paste" && e["text"] == text
        });
        let fence = rig.control(&pane, json!({"command": "barrier"}));
        assert_eq!(fence["state"], "idle");
        assert!(!rig.journal(&pane).iter().any(|e| {
            e["event_sequence"].as_u64().is_some_and(|s| s > cursor)
                && (e["kind"] == "accepted"
                    || e["kind"] == "queued"
                    || e["kind"] == "selection"
                    || e["event"] == "UserPromptSubmit"
                    || e["event"] == "input")
        }));
        let cursor = rig.event_cursor(&pane);
        rig.paste(&pane, "next paste works");
        rig.wait_event(&pane, cursor, |e| {
            e["kind"] == "accepted" && e["text"] == "next paste works"
        });
        let row = rig.wait_for_row(&pane, |r| r.status == atm_core::SessionStatus::Working);
        assert_eq!(row.first_prompt.as_deref(), Some("next paste works"));
        rig.paste(&pane, "/atm/exit");
        rig.wait_for_shell(&pane);
    }
    rig.close();
}

/// The screen can become idle before the real daemon sees the lifecycle end.
#[test]
fn held_completion_waits_for_release_and_preserves_hook_order() {
    let mut rig = StandInRig::start();
    for mode in [StandInMode::Claude, StandInMode::Codex, StandInMode::Pi] {
        let mut config = StandInConfig::new(mode);
        config.manual_clock = true;
        let pane = rig.launch(&config);
        rig.paste(&pane, "held completion");
        rig.wait_for_row(&pane, |r| r.status == atm_core::SessionStatus::Working);
        let expected = if mode == StandInMode::Pi {
            vec!["turn_end", "agent_end"]
        } else {
            vec!["Stop"]
        };
        let cursor = rig.event_cursor(&pane);
        let command = if mode == StandInMode::Pi {
            json!({"command": "hold_next_hook"})
        } else {
            json!({"command": "hold_next_hook", "event": "Stop"})
        };
        rig.control(&pane, command);
        rig.control(&pane, json!({"command": "advance_to", "now_ms": 500}));
        rig.wait_event(&pane, cursor, |e| {
            e["kind"] == "held" && e["event"] == expected[0]
        });
        rig.wait_event(&pane, cursor, |e| {
            e["kind"] == "rendered" && e["state"] == "idle"
        });
        let fence = rig.control(&pane, json!({"command": "barrier"}));
        assert_eq!(fence["state"], "idle");
        let held: Vec<_> = rig
            .journal(&pane)
            .into_iter()
            .filter(|e| e["kind"] == "held")
            .collect();
        assert_eq!(
            held.iter()
                .map(|e| e["event"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(!rig
            .journal(&pane)
            .iter()
            .any(|e| { e["kind"] == "event" && expected.iter().any(|name| e["event"] == *name) }));
        assert_eq!(
            rig.pane_rows(&pane)[0].status,
            atm_core::SessionStatus::Working
        );

        rig.control(&pane, json!({"command": "release_hook"}));
        rig.wait_for_row(&pane, |r| r.status == atm_core::SessionStatus::Idle);
        let forwarded: Vec<_> = rig
            .journal(&pane)
            .into_iter()
            .filter(|e| e["kind"] == "event" && expected.iter().any(|name| e["event"] == *name))
            .collect();
        assert_eq!(
            forwarded.iter().map(|e| &e["data"]).collect::<Vec<_>>(),
            held.iter().map(|e| &e["data"]).collect::<Vec<_>>()
        );
        rig.paste(&pane, "unheld next turn");
        rig.wait_for_row(&pane, |r| r.status == atm_core::SessionStatus::Working);
        rig.control(&pane, json!({"command": "advance_to", "now_ms": 1000}));
        rig.wait_for_row(&pane, |r| r.status == atm_core::SessionStatus::Idle);
        rig.paste(&pane, "/atm/exit");
        rig.wait_for_shell(&pane);
    }
    rig.close();
}

#[test]
fn held_shutdown_keeps_private_control_stream_alive_until_release() {
    let mut rig = StandInRig::start();
    for mode in [StandInMode::Claude, StandInMode::Codex, StandInMode::Pi] {
        let mut config = StandInConfig::new(mode);
        config.manual_clock = true;
        let pane = rig.launch(&config);
        rig.control(&pane, json!({"command": "hold_next_hook"}));
        let cursor = rig.event_cursor(&pane);
        rig.paste(&pane, "/atm/exit");
        rig.wait_event(&pane, cursor, |e| e["kind"] == "held");
        let fence = rig.control(&pane, json!({"command": "barrier"}));
        assert_eq!(fence["state"], "exited");
        assert!(!rig.journal(&pane).iter().any(|e| e["kind"] == "exit"));
        rig.control(&pane, json!({"command": "release_hook"}));
        rig.wait_for_shell(&pane);
    }
    rig.close();
}

/// Synthetic context usage travels through each real adapter, not a registry shortcut.
#[test]
fn next_context_share_reaches_the_daemon_in_every_mode() {
    use atm_core::SessionStatus;
    let mut rig = StandInRig::start();
    for mode in [StandInMode::Claude, StandInMode::Codex, StandInMode::Pi] {
        let mut config = StandInConfig::new(mode);
        config.manual_clock = true;
        let pane = rig.launch(&config);
        rig.paste(&pane, "context line");
        rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
        rig.control(
            &pane,
            json!({"command": "set_context_share", "percent": 40}),
        );
        rig.control(&pane, json!({"command": "advance_to", "now_ms": 500}));
        let reported = rig.wait_for_row(&pane, |r| {
            r.status == SessionStatus::Idle && (r.context_percentage - 40.0).abs() < 0.0001
        });
        assert_eq!(reported.context_percentage, 40.0);

        // An unconfigured next payload is unchanged. Codex has no baseline usage
        // payload, so an unavailable transcript leaves the last known share alone.
        rig.paste(&pane, "unconfigured usage");
        rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
        rig.control(&pane, json!({"command": "advance_to", "now_ms": 1000}));
        rig.wait_for_row(&pane, |r| {
            r.status == SessionStatus::Idle
                && if mode == StandInMode::Codex {
                    r.context_percentage == 40.0
                } else {
                    r.context_percentage < 1.0
                }
        });
        rig.control(&pane, json!({"command": "set_context_share", "percent": 0}));
        rig.paste(&pane, "explicit zero usage");
        rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
        rig.control(&pane, json!({"command": "advance_to", "now_ms": 1500}));
        rig.wait_for_row(&pane, |r| {
            r.status == SessionStatus::Idle && r.context_percentage == 0.0
        });
        rig.paste(&pane, "/atm/exit");
        rig.wait_for_shell(&pane);
    }
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
