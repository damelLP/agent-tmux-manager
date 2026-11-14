//! Mandatory shared harness contract tests. No model CLIs are invoked.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/stand_in.rs"]
mod stand_in;
#[path = "support/stand_in/test_suite.rs"]
mod support_tests;

use atm_core::SessionStatus;
use stand_in::{bounded, StandInConfig, StandInMode, StandInRig};

#[test]
fn stand_in_forwarder_identity() {
    let mut rig = StandInRig::start();
    for mode in [StandInMode::Claude, StandInMode::Codex] {
        let mut config = StandInConfig::new(mode);
        config.manual_clock = true;
        let pane = rig.launch(&config);
        let comm = std::fs::read_to_string(format!("/proc/{}/comm", pane.pid)).unwrap();
        assert_eq!(comm.trim(), mode.name());
        rig.discover();
        if mode == StandInMode::Codex {
            let pending = rig.wait_for_row(&pane, |_| true);
            assert_eq!(pending.id.as_str(), format!("pending-{}", pane.pid));
            rig.paste(&pane, "identity prompt");
        }
        let row = rig.wait_for_row(&pane, |r| !r.id.is_pending());
        assert_eq!(row.tmux_pane.as_deref(), Some(pane.pane_id.as_str()));
        rig.discover();
        assert_eq!(
            rig.pane_rows(&pane).len(),
            1,
            "hook and discovery must share a PID"
        );
        rig.paste(&pane, "/atm/exit");
        rig.wait_for_absent(&row.id);
        rig.wait_for_shell(&pane);
    }
    rig.close();
}

#[test]
fn stand_in_private_server_isolation() {
    let mut first = StandInRig::start();
    let mut second = StandInRig::start();
    let mut config = StandInConfig::new(StandInMode::Claude);
    config.manual_clock = true;
    let a = first.launch(&config);
    let b = second.launch(&config);
    assert_eq!(a.pane_id, b.pane_id);
    assert_ne!(first.socket(), second.socket());
    first.wait_for_row(&a, |r| r.status == SessionStatus::Idle);
    second.wait_for_row(&b, |r| r.status == SessionStatus::Idle);
    first.paste(&a, "only first");
    first.wait_for_row(&a, |r| r.status == SessionStatus::Working);
    assert_eq!(second.pane_rows(&b)[0].status, SessionStatus::Idle);
    first.close();
    assert!(second
        .tmux(&["display-message", "-p", "#{pid}"])
        .status
        .success());
    second.close();
}

#[test]
fn stand_in_missing_prerequisite_is_a_failure() {
    let failure = std::panic::catch_unwind(|| stand_in::require_tools(std::ffi::OsStr::new("")));
    let message = failure.expect_err("missing tools must never skip successfully");
    let message = message.downcast_ref::<String>().unwrap();
    assert!(
        message.contains("required stand-in prerequisite: python3"),
        "{message}"
    );
}

#[test]
fn stand_in_rejects_unsealed_socket_before_starting() {
    let mut rig = StandInRig::start();
    for missing in [false, true] {
        let mut cmd = rig.command(&rig.launcher(StandInMode::Claude));
        cmd.env("ATM_STANDIN_ROOT", rig.root().join("tmp"));
        if missing {
            cmd.env_remove("ATM_SOCKET");
        }
        let output = rig.run(&mut cmd);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("ATM_SOCKET must be"),
            "{output:?}"
        );
    }
    rig.close();
}

fn success(output: std::process::Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn assert_status_after(
    rig: &StandInRig,
    pane: &stand_in::StandInPane,
    status: SessionStatus,
    ms: u64,
) {
    advance(rig, pane, ms);
    rig.wait_ready(pane, None);
    let rows = rig.pane_rows(pane);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].status, status);
}

fn advance(rig: &StandInRig, pane: &stand_in::StandInPane, ms: u64) {
    let now = rig.control(pane, serde_json::json!({"command": "barrier"}))["now_ms"]
        .as_u64()
        .unwrap();
    rig.control(
        pane,
        serde_json::json!({"command": "advance_to", "now_ms": now + ms}),
    );
}

fn paste_frame(
    rig: &StandInRig,
    pane: &stand_in::StandInPane,
    text: &str,
    state: &str,
) -> serde_json::Value {
    let after = rig.event_cursor(pane);
    rig.paste(pane, text);
    rig.wait_event(pane, after, |e| {
        e["kind"] == "rendered" && e["state"] == state && e["draft"] == ""
    })
}

fn event_names(rig: &StandInRig, pane: &stand_in::StandInPane) -> Vec<String> {
    rig.journal(pane)
        .iter()
        .filter(|e| e["kind"] == "event")
        .map(|e| e["event"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn claude_stand_in_lifecycle() {
    let mut rig = StandInRig::start();
    let mut config = StandInConfig::new(StandInMode::Claude);
    config.manual_clock = true;
    config.work_ms = 1200;
    config.clear_gap_ms = 1200;
    let pane = rig.launch(&config);
    let original = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    assert!(rig
        .journal(&pane)
        .iter()
        .any(|e| e["kind"] == "status" && e["data"]["session_id"] == original.id.as_str()));
    let text = "Unicode λ, quote ' and \"\n/clear\ntrailing semicolon;";
    success(rig.atm(&["send", &pane.pane_id, text]));
    let working = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
    assert_eq!(working.first_prompt.as_deref(), Some(text));
    advance(&rig, &pane, 1200);
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.wait_ready(&pane, None);
    paste_frame(&rig, &pane, "/clear", "clearing");
    rig.wait_for_absent(&original.id);
    assert!(rig.capture(&pane).contains("running SessionEnd hook"));
    assert!(
        rig.pane_rows(&pane).is_empty(),
        "SessionEnd must remove the row during the gap"
    );
    // Synthetic: replay a captured-shaped status after SessionEnd but before the new start.
    let after = rig.event_cursor(&pane);
    let submitted = paste_frame(
        &rig,
        &pane,
        &format!("/atm/status {}", original.id),
        "clearing",
    );
    assert_eq!(
        submitted["draft"], "",
        "must await submission, not its draft redraw"
    );
    rig.wait_event(&pane, after, |e| {
        e["kind"] == "status" && e["data"]["session_id"] == original.id.as_str()
    });
    assert!(
        rig.pane_rows(&pane).is_empty(),
        "late status must not resurrect the old row"
    );
    advance(&rig, &pane, 1200);
    let new = rig.wait_for_row(&pane, |r| {
        r.id != original.id && r.status == SessionStatus::Idle
    });
    rig.wait_ready(&pane, Some(new.id.as_str()));
    assert_ne!(original.id, new.id);
    assert!(new.first_prompt.is_none());
    assert!(
        rig.journal(&pane)
            .iter()
            .filter(|e| e["kind"] == "status")
            .count()
            >= 4
    );

    paste_frame(&rig, &pane, "/atm/dialog", "dialog");
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::AttentionNeeded);
    assert!(rig.capture(&pane).contains("Esc to cancel"));
    let prompt = success(rig.atm(&["peek", "--prompt", &pane.pane_id]));
    for visible in [
        "Do you want to proceed?",
        "1. Yes",
        "2. No",
        "Esc to cancel",
    ] {
        assert!(prompt.contains(visible), "{prompt}");
    }
    let accepted_before = rig
        .journal(&pane)
        .iter()
        .filter(|e| e["kind"] == "accepted")
        .count();
    let refused = rig.atm(&["send", &pane.pane_id, "must not enter dialog"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("dialog"));
    assert_eq!(
        accepted_before,
        rig.journal(&pane)
            .iter()
            .filter(|e| e["kind"] == "accepted")
            .count()
    );
    let before_escape = event_names(&rig, &pane);
    rig.key(&pane, "Escape");
    rig.wait_ready(&pane, Some(new.id.as_str()));
    assert_status_after(&rig, &pane, SessionStatus::AttentionNeeded, 1300);
    assert_eq!(event_names(&rig, &pane), before_escape);
    assert!(!rig.capture(&pane).contains("Esc to cancel"));
    assert!(!rig
        .atm(&["send", &pane.pane_id, "stale status still refuses"])
        .status
        .success());
    rig.paste(&pane, "recover directly");
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
    advance(&rig, &pane, 1200);
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.wait_ready(&pane, None);
    paste_frame(&rig, &pane, "/atm/drop-next Stop", "idle");
    rig.paste(&pane, "lose completion");
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
    let after = rig.event_cursor(&pane);
    advance(&rig, &pane, 1200);
    rig.wait_event(&pane, after, |e| {
        e["kind"] == "dropped" && e["event"] == "Stop"
    });
    rig.wait_ready(&pane, None);
    assert_status_after(&rig, &pane, SessionStatus::Working, 100);
    rig.paste(&pane, "/atm/exit");
    rig.wait_for_absent(&new.id);
    rig.wait_for_shell(&pane);
    rig.close();
}

#[test]
fn codex_stand_in_lazy_session() {
    let mut rig = StandInRig::start();
    let mut config = StandInConfig::new(StandInMode::Codex);
    config.manual_clock = true;
    config.work_ms = 1000;
    let pane = rig.launch(&config);
    assert!(event_names(&rig, &pane).is_empty());
    rig.discover();
    let pending = rig.wait_for_row(&pane, |_| true);
    assert_eq!(pending.id.as_str(), format!("pending-{}", pane.pid));
    rig.paste(&pane, "first");
    let old = rig.wait_for_row(&pane, |r| {
        !r.id.is_pending() && r.status == SessionStatus::Working
    });
    advance(&rig, &pane, 1000);
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.wait_ready(&pane, None);
    let before = event_names(&rig, &pane);
    let clearing = paste_frame(&rig, &pane, "/new", "clearing");
    let next = clearing["session_id"].as_str().unwrap().to_owned();
    assert_ne!(next, old.id.as_str());
    advance(&rig, &pane, 100);
    rig.wait_ready(&pane, Some(&next));
    assert_eq!(
        event_names(&rig, &pane),
        before,
        "clear must not announce its ID"
    );
    assert_eq!(rig.pane_rows(&pane)[0].id, old.id);
    rig.paste(&pane, "second exact");
    let new = rig.wait_for_row(&pane, |r| {
        r.id != old.id && r.status == SessionStatus::Working
    });
    assert_eq!(new.id.as_str(), next);
    assert_eq!(new.first_prompt.as_deref(), Some("second exact"));
    let before_escape = event_names(&rig, &pane);
    rig.key(&pane, "Escape");
    rig.wait_ready(&pane, Some(&next));
    assert_status_after(&rig, &pane, SessionStatus::Working, 1100);
    assert_eq!(
        event_names(&rig, &pane),
        before_escape,
        "Esc must cancel the scheduled Stop"
    );
    rig.paste(&pane, "/quit");
    rig.wait_for_absent(&new.id);
    rig.wait_for_shell(&pane);
    assert!(rig.pane_rows(&pane).is_empty());
    rig.close();
}

#[test]
fn pi_stand_in_abort() {
    // Captured-envelope replay: this does not execute pi or its extension.
    let mut rig = StandInRig::start();
    let mut config = StandInConfig::new(StandInMode::Pi);
    config.manual_clock = true;
    config.work_ms = 1000;
    config.clear_gap_ms = 1200;
    let pane = rig.launch(&config);
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.paste(&pane, "pi replay\nexact text");
    let old = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
    assert_eq!(old.first_prompt.as_deref(), Some("pi replay\nexact text"));
    rig.key(&pane, "Escape");
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.wait_ready(&pane, None);
    assert_eq!(
        event_names(&rig, &pane),
        [
            "session_start",
            "input",
            "agent_start",
            "turn_end",
            "agent_end"
        ]
    );
    let journal = rig.journal(&pane);
    let end = journal.iter().find(|e| e["event"] == "agent_end").unwrap();
    assert_eq!(
        end["data"]["payload"]["messages"][0]["stopReason"],
        "aborted"
    );
    paste_frame(&rig, &pane, "/new", "clearing");
    rig.wait_for_absent(&old.id);
    assert!(rig
        .capture(&pane)
        .contains("[fixture1] reply with the word ok"));
    assert!(
        rig.pane_rows(&pane).is_empty(),
        "shutdown must remove the row in the clear gap"
    );
    advance(&rig, &pane, 1200);
    let new = rig.wait_for_row(&pane, |r| r.id != old.id && r.status == SessionStatus::Idle);
    rig.wait_ready(&pane, Some(new.id.as_str()));
    assert_ne!(old.id, new.id);
    rig.paste(&pane, "normal pi turn");
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
    advance(&rig, &pane, 1000);
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.paste(&pane, "/atm/exit");
    rig.wait_for_absent(&new.id);
    rig.wait_for_shell(&pane);
    rig.close();
}

#[test]
fn stand_in_discovery_and_liveness() {
    let mut rig = StandInRig::start();
    let mut config = StandInConfig::new(StandInMode::Claude);
    config.manual_clock = true;
    let pane = rig.launch(&config);
    let row = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.discover();
    assert_status_after(&rig, &pane, SessionStatus::Idle, 2200);
    assert_eq!(rig.pane_rows(&pane)[0].id, row.id);
    // Lose the shutdown hook: actual PID death, not an event, must reap the row.
    paste_frame(&rig, &pane, "/atm/drop-next SessionEnd", "idle");
    rig.paste(&pane, "/atm/exit");
    rig.wait_for_shell(&pane);
    rig.wait_for_absent(&row.id);
    rig.close();
}

#[test]
fn stand_in_spawn_prompt() {
    let mut rig = StandInRig::start();
    let log = rig.root().join("spawn.jsonl");
    for (name, value) in [
        ("ATM_STANDIN_LOG", log.to_str().unwrap()),
        ("ATM_STANDIN_ID_PREFIX", "spawn-fixture"),
        ("ATM_STANDIN_WORK_MS", "1200"),
        ("ATM_STANDIN_MANUAL_CLOCK", "1"),
    ] {
        success(rig.tmux(&["set-environment", "-g", name, value]));
    }
    let target = success(rig.tmux(&["display-message", "-p", "-t", "fixture:0", "#{pane_id}"]));
    let text = "spawn 'quote', \"double quote\", $literal;";
    let mut command = rig.command(std::path::Path::new(env!("CARGO_BIN_EXE_atm")));
    command
        .args([
            "spawn",
            "--target-pane",
            &target,
            "--model",
            "fixture-model",
            "--prompt",
            text,
        ])
        .env("TMUX", rig.tmux_env())
        .env("ATM_SPAWN_BIN", rig.launcher(StandInMode::Claude));
    let pane_id = success(rig.run(&mut command));
    let pane = rig.observe_pane(pane_id, StandInMode::Claude, log);
    let row = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
    assert_eq!(row.first_prompt.as_deref(), Some(text));
    rig.discover();
    assert_eq!(rig.pane_rows(&pane).len(), 1);
    advance(&rig, &pane, 1200);
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.paste(&pane, "/atm/exit");
    rig.wait_for_absent(&row.id);
    rig.wait_for_shell(&pane);
    rig.close();
}

#[test]
fn stand_in_observes_restarted_process() {
    let mut rig = StandInRig::start();
    let first = rig.launch(&StandInConfig::new(StandInMode::Claude));
    let old = rig.wait_for_row(&first, |r| r.status == SessionStatus::Idle);
    rig.paste(&first, "/atm/exit");
    rig.wait_for_shell(&first);
    // tmux reruns the original launch command without shell-exported config.
    success(rig.tmux(&["respawn-pane", "-k", "-t", &first.pane_id]));
    let second = rig.observe_pane(
        first.pane_id.clone(),
        StandInMode::Claude,
        first.log.clone(),
    );
    assert_ne!(
        second.pid, first.pid,
        "must not return the old journal identity"
    );
    rig.wait_ready(&second, None);
    let new = rig.wait_for_row(&second, |r| r.status == SessionStatus::Idle);
    assert_ne!(old.id, new.id);
    // Also relaunch in the retained shell, using exactly the same config/prefix.
    rig.paste(&second, "/atm/exit");
    rig.wait_for_shell(&second);
    rig.paste(
        &second,
        &format!(
            "'{}' --atm-config '{}'",
            second.launcher.display(),
            second.log.with_extension("config.json").display()
        ),
    );
    let third = rig.observe_pane(
        second.pane_id.clone(),
        StandInMode::Claude,
        second.log.clone(),
    );
    rig.wait_ready(&third, None);
    let newest = rig.wait_for_row(&third, |r| r.status == SessionStatus::Idle);
    assert_ne!(newest.id, old.id);
    assert_ne!(newest.id, new.id);
    rig.close();
}

#[test]
fn stand_in_visible_draft_queue_and_interrupt() {
    let mut rig = StandInRig::start();
    let mut config = StandInConfig::new(StandInMode::Claude);
    config.manual_clock = true;
    config.work_ms = 1200;
    let pane = rig.launch(&config);
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "typed draft");
    rig.wait_event(&pane, after, |e| {
        e["kind"] == "rendered" && e["draft"] == "typed draft"
    });
    assert!(rig.capture(&pane).contains("❯\u{a0}typed draft"));
    let after = rig.event_cursor(&pane);
    success(rig.tmux(&["set-buffer", "-b", "draft", " + pasted"]));
    success(rig.tmux(&[
        "paste-buffer",
        "-p",
        "-d",
        "-b",
        "draft",
        "-t",
        &pane.pane_id,
    ]));
    rig.wait_event(&pane, after, |e| {
        e["kind"] == "rendered" && e["draft"] == "typed draft + pasted"
    });
    assert!(rig.capture(&pane).contains("❯\u{a0}typed draft + pasted"));
    rig.key(&pane, "C-u");
    rig.paste(&pane, "interrupt me");
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
    let after = rig.event_cursor(&pane);
    success(rig.atm(&["interrupt", &pane.pane_id]));
    rig.wait_event(&pane, after, |e| {
        e["kind"] == "rendered" && e["draft"] == "interrupt me"
    });
    assert!(rig.capture(&pane).contains("❯\u{a0}interrupt me"));
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "C-c");
    rig.wait_event(&pane, after, |e| {
        e["kind"] == "rendered" && e["draft"] == ""
    });
    assert!(rig
        .capture(&pane)
        .lines()
        .any(|line| line.trim_end() == "❯"));
    paste_frame(&rig, &pane, "first queued test", "working");
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
    let after = rig.event_cursor(&pane);
    rig.paste(&pane, "second queued test");
    rig.wait_event(&pane, after, |e| {
        e["kind"] == "queued" && e["text"] == "second queued test"
    });
    assert!(!rig
        .journal(&pane)
        .iter()
        .any(|e| e["event"] == "UserPromptSubmit" && e["data"]["prompt"] == "second queued test"));
    let after = rig.event_cursor(&pane);
    advance(&rig, &pane, 1200);
    // Diagnostic send attempt only; this synthetic FIFO is not real Claude delivery.
    rig.wait_event(&pane, after, |e| {
        e["event"] == "UserPromptSubmit" && e["data"]["prompt"] == "second queued test"
    });
    advance(&rig, &pane, 1200);
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.wait_ready(&pane, None);
    paste_frame(&rig, &pane, "/atm/dialog", "dialog");
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::AttentionNeeded);
    let after = rig.event_cursor(&pane);
    success(rig.atm(&["reply", &pane.pane_id, "--option", "2"]));
    rig.wait_event(&pane, after, |e| {
        e["kind"] == "selection" && e["option"] == 2
    });
    rig.paste(&pane, "/atm/exit");
    rig.wait_for_shell(&pane);
    rig.close();
}

#[test]
fn pi_stand_in_reconnects_without_reconnecting_on_clear() {
    let mut rig = StandInRig::start();
    let mut config = StandInConfig::new(StandInMode::Pi);
    config.manual_clock = true;
    let pane = rig.launch(&config);
    let old = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    paste_frame(&rig, &pane, "/new", "clearing");
    advance(&rig, &pane, 100);
    let new = rig.wait_for_row(&pane, |r| r.id != old.id);
    rig.wait_ready(&pane, Some(new.id.as_str()));
    let connections = |rig: &StandInRig| {
        rig.journal(&pane)
            .iter()
            .filter(|e| e["kind"] == "connected")
            .count()
    };
    assert_eq!(connections(&rig), 1, "one socket across /new");
    let after = rig.event_cursor(&pane);
    rig.stop_daemon();
    rig.wait_event(&pane, after, |e| e["kind"] == "disconnected");
    // Wait on the transport timer event, not model-clock advancement or a
    // fixed delay. Exact in-backoff drop boundaries are covered by pure tests;
    // real input cannot be guaranteed to arrive within 1s on a loaded host.
    rig.wait_event(&pane, after, |e| e["kind"] == "transport_retry_ready");
    rig.start_daemon();
    assert_eq!(
        connections(&rig),
        1,
        "backoff expiry alone must not reconnect"
    );
    rig.paste(&pane, "new event after backoff");
    rig.wait_for_row(&pane, |r| {
        r.first_prompt.as_deref() == Some("new event after backoff")
    });
    advance(&rig, &pane, 500);
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    assert_eq!(
        rig.journal(&pane)
            .iter()
            .filter(|e| e["kind"] == "connected")
            .count(),
        2
    );
    rig.paste(&pane, "/atm/exit");
    rig.wait_for_shell(&pane);
    rig.close();
}

#[test]
fn startup_diagnostics_include_private_pane_errors() {
    let mut rig = StandInRig::start();
    let pane = success(rig.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "printf 'stand-in-startup-error\\n'; exec /bin/bash --noprofile --norc",
    ]));
    rig.wait("error marker", || {
        success(rig.tmux(&["capture-pane", "-p", "-t", &pane])).contains("stand-in-startup-error")
    });
    assert!(rig.diagnostics().contains("stand-in-startup-error"));
    // Synthetic: exercise the diagnostic for a lost real-forwarder handshake.
    std::fs::write(rig.root().join("lost.log"), "Handshake failed: Broken pipe").unwrap();
    assert!(rig.diagnostics().contains("Possible lost hook"));
    rig.close();
}

#[test]
fn protocol_request_is_bounded_when_listener_never_answers() {
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("silent.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (accepted, wait_accepted) = std::sync::mpsc::channel();
    let (release, wait_release) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        accepted.send(()).unwrap();
        wait_release.recv_timeout(Duration::from_secs(60)).unwrap();
    });
    let request = std::thread::spawn(move || {
        stand_in::protocol_request(
            &socket,
            atm_protocol::ClientMessage::list_sessions(),
            Duration::from_secs(30),
        )
    });
    wait_accepted.recv_timeout(Duration::from_secs(30)).unwrap();
    let error = request
        .join()
        .unwrap()
        .expect_err("a stalled accepted peer must time out");
    release.send(()).unwrap();
    peer.join().unwrap();
    assert!(error.to_string().contains("deadline"), "{error}");
}

#[test]
fn stand_in_daemon_control_and_auto_start_cleanup() {
    let mut rig = StandInRig::start();
    let root = rig.root().to_owned();
    let launcher = rig.launcher(StandInMode::Claude);
    rig.set_daemon_env("ATM_SPAWN_BIN", &launcher);
    rig.restart_daemon();
    let pid = rig.daemon_pids()[0];
    let env = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    assert!(env
        .split(|b| *b == 0)
        .any(|v| v == format!("ATM_SPAWN_BIN={}", launcher.display()).as_bytes()));
    rig.stop_daemon();
    assert!(rig.daemon_pids().is_empty());
    // A real CLI auto-start, not a child inserted into the rig's bookkeeping.
    success(rig.atm(&["list"]));
    assert_eq!(rig.daemon_pids().len(), 1);
    let auto = rig.daemon_pids()[0];
    rig.close();
    assert!(rig.daemon_pids().is_empty());
    drop(rig);
    assert!(!root.exists());
    assert!(!std::fs::read(format!("/proc/{auto}/environ"))
        .unwrap_or_default()
        .split(|b| *b == 0)
        .any(|v| v.starts_with(b"ATM_SOCKET=")));
}

#[test]
fn stand_in_explicit_launcher_without_cli_spawn_override() {
    let mut rig = StandInRig::start();
    let log = rig.root().join("explicit-launcher.jsonl");
    let config = rig.root().join("explicit-launcher.json");
    std::fs::write(
        &config,
        serde_json::json!({
            "LOG": log, "ID_PREFIX": "explicit-launcher", "MANUAL_CLOCK": 1,
        })
        .to_string(),
    )
    .unwrap();
    // Same explicit executable/config route a daemon command builder can use;
    // no atm CLI, installed launcher, PATH override or ATM_SPAWN_BIN involved.
    let pane_id = success(rig.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        rig.launcher(StandInMode::Claude).to_str().unwrap(),
        "--atm-config",
        config.to_str().unwrap(),
    ]));
    let pane = rig.observe_pane(pane_id, StandInMode::Claude, log);
    rig.wait_ready(&pane, None);
    let row = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    assert_eq!(row.tmux_pane.as_deref(), Some(pane.pane_id.as_str()));
    let env = std::fs::read(format!("/proc/{}/environ", pane.pid)).unwrap();
    assert!(!env
        .split(|b| *b == 0)
        .any(|entry| entry.starts_with(b"ATM_SPAWN_BIN=")));
    rig.close();
}

#[test]
fn stand_in_invalid_arguments_do_not_announce() {
    let mut rig = StandInRig::start();
    let log = rig.root().join("invalid.jsonl");
    let mut cmd = rig.command(&rig.launcher(StandInMode::Claude));
    cmd.env("ATM_STANDIN_LOG", &log)
        .env("ATM_STANDIN_ID_PREFIX", "invalid")
        .env("TMUX", rig.tmux_env())
        .env("TMUX_PANE", "%0")
        .args(["one", "two"]);
    let output = rig.run(&mut cmd);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("at most one positional"));
    assert!(
        !log.exists(),
        "invalid arguments must fail before identity or SessionStart"
    );
    rig.close();
}

#[test]
fn stand_in_symlinked_tmpdir_cleanup() {
    if std::env::var_os("STANDIN_SYMLINK_CHILD").is_some() {
        let mut rig = StandInRig::start();
        let pane = rig.launch(&StandInConfig::new(StandInMode::Claude));
        rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
        rig.parent_ready(); // Parent owns the root before resources start, then holds this barrier.
        rig.close();
        return;
    }
    for forced_exit in [false, true] {
        let mut guard = stand_in::parent::ChildGuard::spawn("stand_in_symlinked_tmpdir_cleanup");
        guard.wait_ready();
        assert!(
            guard.child_root().exists(),
            "guardian retains the child's socket directory"
        );
        guard.finish(forced_exit);
        assert_eq!(std::fs::read_dir(guard.real()).unwrap().count(), 0);
    }
}

#[test]
fn stand_in_parent_guard_cleans_on_parent_unwind() {
    let guard = stand_in::parent::ChildGuard::spawn("stand_in_symlinked_tmpdir_cleanup");
    let base = guard.real().parent().unwrap().to_owned();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut guard = guard;
        guard.wait_ready();
        panic!("synthetic parent assertion after child resources started");
    }));
    assert!(result.is_err());
    assert!(
        !base.exists(),
        "guardian must stop resources before deleting its base"
    );
}

#[test]
fn stand_in_readiness_does_not_require_tmux_paste_format() {
    let mut rig = StandInRig::start();
    let pane = rig.launch(&StandInConfig::new(StandInMode::Claude));
    let row = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    // Decouple readiness from the optional tmux format, even on tmux >= 3.7.
    let tty = success(rig.tmux(&["display-message", "-p", "-t", &pane.pane_id, "#{pane_tty}"]));
    std::fs::write(tty, b"\x1b[?2004l").unwrap();
    rig.wait_ready(&pane, Some(row.id.as_str()));
    rig.close();
}

#[test]
fn stand_in_model_contract() {
    let output = bounded::run(
        std::process::Command::new("python3")
            .args([
                "-B",
                "-m",
                "unittest",
                "-v",
                "test_model",
                "test_runtime",
                "test_screens",
                "test_dialogs",
                "test_scene",
                "test_pi_replay",
                "test_hook",
            ])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .current_dir(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/stand_in"
            )),
    );
    assert!(output.status.success(), "{output:?}");
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
}
