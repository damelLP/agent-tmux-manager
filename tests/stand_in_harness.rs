//! Mandatory shared harness contract tests. No model CLIs are invoked.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/stand_in.rs"]
mod stand_in;

use atm_core::SessionStatus;
use stand_in::{bounded, StandInConfig, StandInMode, StandInRig};

#[test]
fn stand_in_forwarder_identity() {
    let mut rig = StandInRig::start();
    for mode in [StandInMode::Claude, StandInMode::Codex] {
        let pane = rig.launch(&StandInConfig::new(mode));
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
    let a = first.launch(&StandInConfig::new(StandInMode::Claude));
    let b = second.launch(&StandInConfig::new(StandInMode::Claude));
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

fn assert_status_for(
    rig: &StandInRig,
    pane: &stand_in::StandInPane,
    status: SessionStatus,
    ms: u64,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        let rows = rig.pane_rows(pane);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].status, status);
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
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
    config.work_ms = 1200;
    config.clear_gap_ms = 1200;
    let pane = rig.launch(&config);
    let original = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    let text = "Unicode λ, quote ' and \"\n/clear\ntrailing semicolon;";
    success(rig.atm(&["send", &pane.pane_id, text]));
    let working = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
    assert_eq!(working.first_prompt.as_deref(), Some(text));
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.wait_ready(&pane, None);
    rig.paste(&pane, "/clear");
    rig.wait("clear phase", || {
        rig.journal(&pane).iter().any(|e| e["state"] == "clearing")
    });
    rig.wait_for_absent(&original.id);
    let new = rig.wait_for_row(&pane, |r| {
        r.id != original.id && r.status == SessionStatus::Idle
    });
    rig.wait_ready(&pane, Some(new.id.as_str()));
    assert!(new.first_prompt.is_none());

    rig.paste(&pane, "/atm/dialog");
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::AttentionNeeded);
    rig.wait("dialog screen", || {
        rig.capture(&pane).contains("Esc to cancel")
    });
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
    assert_status_for(&rig, &pane, SessionStatus::AttentionNeeded, 1300);
    assert_eq!(event_names(&rig, &pane), before_escape);
    assert!(!rig.capture(&pane).contains("Esc to cancel"));
    assert!(!rig
        .atm(&["send", &pane.pane_id, "stale status still refuses"])
        .status
        .success());
    rig.paste(&pane, "recover directly");
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.wait_ready(&pane, None);
    rig.paste(&pane, "/atm/drop-next Stop");
    rig.wait("drop armed", || rig.capture(&pane).contains("drop armed"));
    rig.paste(&pane, "lose completion");
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
    rig.wait("Stop dropped", || {
        rig.journal(&pane)
            .iter()
            .any(|e| e["kind"] == "dropped" && e["event"] == "Stop")
    });
    rig.wait_ready(&pane, None);
    assert_status_for(&rig, &pane, SessionStatus::Working, 100);
    rig.paste(&pane, "/atm/exit");
    rig.wait_for_absent(&new.id);
    rig.wait_for_shell(&pane);
    rig.close();
}

#[test]
fn codex_stand_in_lazy_session() {
    let mut rig = StandInRig::start();
    let mut config = StandInConfig::new(StandInMode::Codex);
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
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.wait_ready(&pane, None);
    let before = event_names(&rig, &pane);
    rig.paste(&pane, "/new");
    let next = format!("{}2", old.id.as_str().strip_suffix('1').unwrap());
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
    assert_status_for(&rig, &pane, SessionStatus::Working, 1100);
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
    config.work_ms = 1000;
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
    rig.paste(&pane, "/new");
    rig.wait_for_absent(&old.id);
    let new = rig.wait_for_row(&pane, |r| r.id != old.id && r.status == SessionStatus::Idle);
    rig.wait_ready(&pane, Some(new.id.as_str()));
    rig.paste(&pane, "normal pi turn");
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
    rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.paste(&pane, "/atm/exit");
    rig.wait_for_absent(&new.id);
    rig.wait_for_shell(&pane);
    rig.close();
}

#[test]
fn stand_in_discovery_and_liveness() {
    let mut rig = StandInRig::start();
    let pane = rig.launch(&StandInConfig::new(StandInMode::Claude));
    let row = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
    rig.discover();
    assert_status_for(&rig, &pane, SessionStatus::Idle, 2200);
    assert_eq!(rig.pane_rows(&pane)[0].id, row.id);
    // Lose the shutdown hook: actual PID death, not an event, must reap the row.
    rig.paste(&pane, "/atm/drop-next SessionEnd");
    rig.wait("drop armed", || rig.capture(&pane).contains("drop armed"));
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
    ] {
        success(rig.tmux(&["set-environment", "-g", name, value]));
    }
    let target = success(rig.tmux(&["display-message", "-p", "-t", "fixture:0", "#{pane_id}"]));
    let text = "spawn 'quote', \"double quote\", $literal;";
    let mut command = rig.command(std::path::Path::new(env!("CARGO_BIN_EXE_atm")));
    command
        .args(["spawn", "--target-pane", &target, "--prompt", text])
        .env("TMUX", rig.tmux_env())
        .env("ATM_SPAWN_BIN", rig.launcher(StandInMode::Claude));
    let pane_id = success(rig.run(&mut command));
    let pane = rig.observe_pane(pane_id, StandInMode::Claude, log);
    let row = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
    assert_eq!(row.first_prompt.as_deref(), Some(text));
    rig.discover();
    assert_eq!(rig.pane_rows(&pane).len(), 1);
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
    rig.paste(&first, "/atm/exit");
    rig.wait_for_shell(&first);
    // Reuse the existing pane's exported fixture env and journal, as respawn does.
    let launcher = first.launcher.to_str().unwrap().replace('\'', "'\\''");
    rig.paste(
        &first,
        &format!("ATM_STANDIN_ID_PREFIX=respawn '{launcher}'"),
    );
    let second = rig.observe_pane(
        first.pane_id.clone(),
        StandInMode::Claude,
        first.log.clone(),
    );
    assert_ne!(
        second.pid, first.pid,
        "must not return the old journal identity"
    );
    rig.wait_ready(&second, Some("respawn-1"));
    rig.wait_for_row(&second, |r| r.id.as_str() == "respawn-1");
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
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("silent.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let before = Instant::now();
    let error = stand_in::protocol_request(
        &socket,
        atm_protocol::ClientMessage::list_sessions(),
        Duration::from_millis(30),
    )
    .expect_err("a silent peer must time out");
    assert!(error.to_string().contains("deadline"), "{error}");
    assert!(before.elapsed() < Duration::from_secs(2));
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
            .args(["-B", "-m", "unittest", "-v", "test_model"])
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
