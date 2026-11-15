//! Real-daemon agent lifecycle acceptance using the shared, model-free stand-in.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/stand_in.rs"]
mod stand_in;
#[path = "support/agent_lifecycle.rs"]
mod support;

use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};

use atm_core::SessionStatus;
use serde_json::json;
use stand_in::StandInMode;
use support::{checked, process_identity, AgentRig, Started};

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
    // Down needs no ending hook or editor input. EOF before SIGHUP can make
    // the stand-in record SessionEnd; if its hook returns first, it records exit.
    for event in test
        .rig
        .journal(&pane)
        .iter()
        .filter(|e| e["event_sequence"].as_u64().unwrap() > before_kill)
    {
        assert!(
            event["kind"] == "exit" || (event["kind"] == "event" && event["event"] == "SessionEnd"),
            "unexpected record after kill: {event}"
        );
    }
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
    // A submitted /exit in one input chunk can leave only the allowed journal
    // records, so the kill must also be the only audited control action.
    assert_eq!(
        events
            .iter()
            .filter(|e| e["kind"].as_str().unwrap().starts_with("control."))
            .count(),
        1,
        "{events:?}"
    );
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
    // Re-read after exit and the intervening checks, without waiting for an
    // optional End hook: a recorded send attempt does not prove its delivery.
    let agent = test.agent(id);
    assert_eq!(agent["state"], "down", "{agent}");
    assert_eq!(agent["pane"], json!(null), "{agent}");
    assert_eq!(agent["session_id"], json!(null), "{agent}");
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
    let screen = test.rig.capture_styled(&pane);
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
    assert_eq!(test.rig.capture_styled(&pane), screen);
    test.rig.close();
}

/// Verify private selectors/env and reject harness executables on daemon PATH.
/// The rig's command() clears every child's environment, including TMUX/TMUX_PANE.
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
        let path = entries
            .iter()
            .find_map(|entry| entry.strip_prefix(b"PATH="))
            .expect("daemon PATH must be explicit");
        for directory in std::env::split_paths(std::ffi::OsStr::from_bytes(path)) {
            assert!(
                directory.is_absolute(),
                "nonabsolute PATH entry: {directory:?}"
            );
            for harness in ["claude", "codex", "pi"] {
                let executable = directory.join(harness);
                match std::fs::metadata(&executable) {
                    Ok(metadata) => assert!(
                        !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0,
                        "harness executable on daemon PATH: {executable:?}"
                    ),
                    Err(error) => assert_eq!(
                        error.kind(),
                        std::io::ErrorKind::NotFound,
                        "cannot inspect daemon PATH candidate {executable:?}: {error}"
                    ),
                }
            }
        }
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

// Ignored cases below are acceptance inventory, NOT implemented assertions.
// Remove an ignore only with its real test body; empty bodies prove nothing.
// Task numbers below are the lifecycle slice's task numbers.

/// Claude up: daemon ATM_SPAWN_BIN, ATM_AGENT, resulting pane tag and bound
/// Idle session, no implicit prompt, all in its own detached placement window.
/// Uses the stand-in's styled fresh-editor Claude frame at 100x30.
#[test]
fn agent_up_ready_tagged_env_claude() {
    up_is_ready_tagged_and_announced(StandInMode::Claude, true);
}

/// Codex up: daemon override, ATM_AGENT and resulting pane tag; Ready/null
/// without submitting a prompt to manufacture a session (100x30 stand-in).
#[test]
fn agent_up_ready_tagged_env_codex() {
    up_is_ready_tagged_and_announced(StandInMode::Codex, false);
}

/// Pi up: daemon override, ATM_AGENT, resulting pane tag and bound Idle
/// session on Ready, no implicit initial prompt (200x50 stand-in).
#[test]
fn agent_up_ready_tagged_env_pi() {
    up_is_ready_tagged_and_announced(StandInMode::Pi, true);
}

/// `agent.up` from a pane of the private server: the daemon splits it, tags
/// the new pane, types the launch command into its shell and waits for the
/// harness. `bound`: the harness announces a session at its start, and Ready
/// is that session with a safe editor; otherwise Ready has no session yet.
fn up_is_ready_tagged_and_announced(mode: StandInMode, bound: bool) {
    let mut test = AgentRig::start(mode);
    let config = support::started_config(mode);
    let started = Started::up(&test, "lead", &config);
    let (id, pane) = (started.id.as_str(), &started.pane);
    assert_eq!(started.result["state"], "ready", "{}", started.result);
    assert_eq!(id, "project/lead");

    // The pane is the lower half of the caller's window: never attached,
    // the size the stand-in draws at, tagged for the agent.
    assert_ne!(pane.pane_id, started.placement);
    let window = |pane_id: &str| {
        checked(test.rig.tmux(&[
            "display-message",
            "-p",
            "-t",
            pane_id,
            "#{window_id} #{window_panes} #{session_attached}",
        ]))
    };
    assert_eq!(window(&pane.pane_id), window(&started.placement));
    assert!(
        window(&pane.pane_id).ends_with(" 2 0"),
        "{}",
        window(&pane.pane_id)
    );
    let size = checked(test.rig.tmux(&[
        "display-message",
        "-p",
        "-t",
        &pane.pane_id,
        "#{pane_width} #{pane_height}",
    ]));
    assert_eq!(size, format!("{} {}", config.columns, config.rows));
    assert_eq!(test.tag(&pane.pane_id), id);

    // The harness is a job of the pane's interactive shell, started through
    // the daemon's ATM_SPAWN_BIN in the role's directory, with ATM_AGENT set.
    let shell: u32 =
        checked(
            test.rig
                .tmux(&["display-message", "-p", "-t", &pane.pane_id, "#{pane_pid}"]),
        )
        .parse()
        .unwrap();
    assert_ne!(shell, pane.pid);
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pane.pid)).unwrap();
    let parent: u32 = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(parent, shell);
    let environ = std::fs::read(format!("/proc/{}/environ", pane.pid)).unwrap();
    assert!(
        environ
            .split(|byte| *byte == 0)
            .any(|entry| entry == format!("ATM_AGENT={id}").as_bytes()),
        "ATM_AGENT is not in the harness's environment"
    );
    assert_eq!(
        std::fs::read_link(format!("/proc/{}/cwd", pane.pid)).unwrap(),
        test.project.canonicalize().unwrap()
    );
    let journal = test.rig.journal(pane);
    let identity = journal.iter().find(|e| e["kind"] == "identity").unwrap();
    assert_eq!(
        identity["argv"][0],
        test.rig.launcher(mode).to_str().unwrap(),
        "{identity}"
    );

    // Ready is the harness's own row and nothing typed into its editor.
    let agent = test.agent(id);
    assert_eq!(agent["state"], "ready", "{agent}");
    assert_eq!(agent["pane"], pane.pane_id);
    if bound {
        let row = test.rig.wait_for_row(pane, |r| {
            !r.id.is_pending() && r.status == SessionStatus::Idle
        });
        assert_eq!(test.rig.pane_rows(pane).len(), 1);
        assert_eq!(agent["session_id"], row.id.as_str(), "{agent}");
    } else {
        // No session is announced before a first prompt: the only row a
        // discovery pass can give the pane is its own placeholder.
        test.rig.discover();
        let rows = test.rig.pane_rows(pane);
        assert!(rows.iter().all(|row| row.id.is_pending()), "{rows:?}");
        assert_eq!(agent["session_id"], json!(null), "{agent}");
        assert_eq!(test.agent(id)["state"], "ready");
    }
    test.assert_no_prompt(pane);
    // Nothing was typed into the caller's pane. (The split resized it, and
    // the rig's shell redraws its prompt as an empty line then.)
    let caller = checked(
        test.rig
            .tmux(&["capture-pane", "-p", "-t", &started.placement]),
    );
    assert!(
        caller.is_empty() || caller == started.placement_screen,
        "{caller}"
    );
    assert_eq!(test.tag(&started.placement), "");

    // The record: requested, the pane, the launch's audit, then readiness;
    // the launch is the only control action.
    let events = test.tail(started.cursor);
    let kinds: Vec<_> = events
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .filter(|kind| kind.starts_with("agent.") || kind.starts_with("control."))
        .collect();
    assert_eq!(
        kinds,
        [
            "agent.up_requested",
            "agent.pane_created",
            "control.spawned",
            if bound {
                "agent.session_bound"
            } else {
                "agent.editor_ready"
            },
        ],
        "{events:?}"
    );
    let spawned = events
        .iter()
        .find(|e| e["kind"] == "control.spawned")
        .unwrap();
    assert_eq!(
        spawned["body"],
        json!({
            "pane": pane.pane_id,
            "action": {
                "method": "agent.up",
                "agent": id,
                "binary": test.rig.launcher(mode),
            },
        })
    );
    // A second up of an agent that is up is refused, with nothing done.
    let cursor = test.cursor();
    let again = test.up_from("lead", &started.placement);
    assert!(again.is_err(), "{again:?}");
    assert!(test.tail(cursor).is_empty());
    assert!(window(&pane.pane_id).ends_with(" 2 0"));
    test.rig.close();
}

/// A linked checkout uses the main role, not a conflicting linked role. Scrub
/// all GIT_* for fixture git commands; prove the temp gitdir before any write.
#[test]
#[ignore = "covered by unit linked_root_uses_main_role; stand-in acceptance needs a scrubbed linked-checkout Git fixture"]
fn agent_up_reads_main_checkout_role() {}

/// Held Claude clear stays Clearing until the stand-in's clock is released;
/// return a new Idle session ID on the same process, with the old row removed.
/// Uses the stand-in's styled fresh-editor Claude frame at 100x30.
#[test]
fn clear_returns_new_ready_session_claude() {
    clear_returns_a_new_ready_session(StandInMode::Claude);
}

/// Held pi clear stays Clearing until the stand-in's clock is released;
/// return a changed Idle session ID on the same process, without a prompt.
#[test]
fn clear_returns_new_ready_session_pi() {
    clear_returns_a_new_ready_session(StandInMode::Pi);
}

/// `agent.clear` of an agent that `agent.up` started and that has had a
/// turn: the clear command goes into the same process, the agent is Clearing
/// for as long as the harness has not announced another session, and the
/// call answers with that session once its editor is safe.
fn clear_returns_a_new_ready_session(mode: StandInMode) {
    let mut test = AgentRig::start(mode);
    let config = support::started_config(mode);
    let started = Started::up(&test, "lead", &config);
    let (id, pane) = (started.id.as_str(), &started.pane);
    let old = test.complete_fixture_turn(pane);
    assert_eq!(test.agent(id)["session_id"], old.id.as_str());
    let identity = process_identity(pane.pid);
    let cursor = test.cursor();
    let after = test.rig.event_cursor(pane);
    let call = test.call_in_background("agent.clear", json!({"agent": id, "timeout_ms": 30000}));
    // The stand-in took the command and holds the clear on its manual clock:
    // the call has not answered, and the agent is Clearing, not Ready.
    test.rig.wait_event(pane, after, |e| {
        e["kind"] == "rendered" && e["state"] == "clearing"
    });
    assert!(!call.is_finished());
    let held = test.agent(id);
    assert_eq!(held["state"], "clearing", "{held}");
    let now = test.rig.control(pane, json!({"command":"barrier"}))["now_ms"]
        .as_u64()
        .unwrap();
    let reply = test.rig.control(
        pane,
        json!({"command":"advance_to", "now_ms": now + config.clear_gap_ms}),
    );
    assert!(reply["error"].is_null(), "{reply}");
    let result = call.join().unwrap().unwrap();
    let row = test.rig.wait_for_row(pane, |r| {
        !r.id.is_pending() && r.id != old.id && r.status == SessionStatus::Idle
    });
    assert_eq!(
        result,
        json!({"agent_id": id, "session_id": row.id.as_str()})
    );
    // The same process in the same pane, one row, and the agent Ready on it.
    assert_eq!(process_identity(pane.pid), identity);
    test.rig.wait_for_absent(&old.id);
    assert_eq!(test.rig.pane_rows(pane).len(), 1);
    let agent = test.agent(id);
    assert_eq!(agent["state"], "ready", "{agent}");
    assert_eq!(agent["pane"], pane.pane_id);
    assert_eq!(agent["session_id"], row.id.as_str());
    // The clear command was the only thing typed, and it was not a prompt.
    test.rig.control(pane, json!({"command": "barrier"}));
    for event in test
        .rig
        .journal(pane)
        .iter()
        .filter(|e| e["event_sequence"].as_u64().unwrap() > after)
    {
        assert!(
            !matches!(
                event["kind"].as_str(),
                Some("identity" | "accepted" | "rejected" | "queued")
            ),
            "{event}"
        );
        assert!(
            !matches!(event["event"].as_str(), Some("UserPromptSubmit" | "input")),
            "{event}"
        );
    }
    let events = test.tail(cursor);
    let kinds: Vec<_> = events
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .filter(|kind| kind.starts_with("agent.") || kind.starts_with("control."))
        .collect();
    assert_eq!(
        kinds,
        ["agent.clear_requested", "agent.session_bound"],
        "{events:?}"
    );
    test.rig.close();
}

/// Codex conversation-to-fresh clear returns Ready/null, excludes the old ID,
/// then binds the next real prompt's ID once without any slash concatenation.
#[test]
#[ignore = "agent.clear is in the base; Codex lazy-session clear stand-in acceptance not yet written"]
fn codex_clear_ready_without_id_then_binds_next_prompt() {}

/// Ready/null with no eligible row AND fresh safe facts sends no clear input
/// and emits no clear_requested; an unreported conversation must not pass.
#[test]
#[ignore = "agent.clear is in the base; Codex clear no-op stand-in acceptance not yet written"]
fn codex_ready_unbound_clear_is_noop() {}

/// Accept only the exact default current-worktree choice, create no checkout,
/// confirm a fresh safe editor and the specified control audit.
#[test]
#[ignore = "agent.clear is in the base; worktree clear stand-in acceptance not yet written"]
fn clear_worktree_default() {}

/// Respawn replaces pane/process/session under the same agent, then down kills
/// the replacement. Use another pre-sized placement window, never split a stand-in.
/// Stale completion is covered by unit stale_completion_after_respawn_cannot_bind.
#[test]
fn respawn_uses_new_process_and_session() {
    let mode = StandInMode::Pi;
    let mut test = AgentRig::start(mode);
    let config = support::started_config(mode);
    let started = Started::up(&test, "lead", &config);
    let (id, old) = (started.id.as_str(), &started.pane);
    let old_row = test.complete_fixture_turn(old);
    let old_identity = process_identity(old.pid);

    // Called from another window: the old pane is killed, never split.
    let log = test.next_stand_in("lead-again", &config);
    let placement = test.placement(&config);
    let cursor = test.cursor();
    let result = test.rpc(
        "agent.respawn",
        json!({"agent": id, "caller_server": test.server()}),
        Some(placement.clone()),
    );
    assert_eq!(result["agent_id"], id);
    let pane_id = result["pane"].as_str().unwrap().to_owned();
    assert_ne!(pane_id, old.pane_id);
    let new = test.rig.observe_pane(pane_id, mode, log);

    // The old pane and its process are gone; the new pane is in the
    // caller's window, tagged, with another process and another session.
    stand_in::process::wait_exit(old.pid, std::time::Duration::from_secs(30)).unwrap();
    let panes = checked(test.rig.tmux(&["list-panes", "-a", "-F", "#{pane_id}"]));
    assert!(!panes.lines().any(|p| p == old.pane_id), "{panes}");
    assert!(panes.lines().any(|p| p == started.placement), "{panes}");
    let window = |pane_id: &str| {
        checked(test.rig.tmux(&[
            "display-message",
            "-p",
            "-t",
            pane_id,
            "#{window_id} #{window_panes} #{session_attached}",
        ]))
    };
    assert_eq!(window(&new.pane_id), window(&placement));
    assert!(window(&new.pane_id).ends_with(" 2 0"));
    assert_eq!(test.tag(&new.pane_id), id);
    assert_ne!(process_identity(new.pid), old_identity);
    let row = test.rig.wait_for_row(&new, |r| {
        !r.id.is_pending() && r.status == SessionStatus::Idle
    });
    assert_ne!(row.id, old_row.id);
    let agent = test.agent(id);
    assert_eq!(agent["state"], "ready", "{agent}");
    assert_eq!(agent["pane"], new.pane_id);
    assert_eq!(agent["session_id"], row.id.as_str());
    test.assert_no_prompt(&new);

    // One record: the kill under agent.respawn, the Down, then a start.
    let events = test.tail(cursor);
    let kinds: Vec<_> = events
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .filter(|kind| kind.starts_with("agent.") || kind.starts_with("control."))
        .collect();
    assert_eq!(
        kinds,
        [
            "control.killed",
            "agent.down",
            "agent.up_requested",
            "agent.pane_created",
            "control.spawned",
            "agent.session_bound",
        ],
        "{events:?}"
    );
    let killed = events
        .iter()
        .find(|e| e["kind"] == "control.killed")
        .unwrap();
    assert_eq!(
        killed["body"],
        json!({"pane": old.pane_id, "action": {"method": "agent.respawn", "agent": id}})
    );
    let bound = events
        .iter()
        .find(|e| e["kind"] == "agent.session_bound")
        .unwrap();
    assert_eq!(bound["body"]["session_id"], row.id.as_str());

    // The agent that was started twice is taken down like any other.
    let cursor = test.cursor();
    let result = test.rpc("agent.down", json!({"agent": id, "kill": true}), None);
    assert_eq!(result, json!({"agent_id": id}));
    stand_in::process::wait_exit(new.pid, std::time::Duration::from_secs(30)).unwrap();
    let panes = checked(test.rig.tmux(&["list-panes", "-a", "-F", "#{pane_id}"]));
    assert!(!panes.lines().any(|p| p == new.pane_id), "{panes}");
    let agent = test.agent(id);
    assert_eq!(agent["state"], "down", "{agent}");
    assert_eq!(agent["pane"], json!(null));
    assert_eq!(
        agent["reason"],
        format!("taken down; {} killed", new.pane_id)
    );
    let events = test.tail(cursor);
    let killed: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "control.killed")
        .collect();
    assert_eq!(killed.len(), 1, "{events:?}");
    assert_eq!(
        killed[0]["body"],
        json!({"pane": new.pane_id, "action": {"method": "agent.down", "agent": id}})
    );
    test.rig.close();
}

/// A daemon restart retains durable identity/binding and converges after live
/// row and foreground-baseline revalidation, without lifecycle input.
#[test]
#[ignore = "covered by unit restart_rederives_baseline_with_live_row; real-daemon restart/list acceptance not yet written"]
fn restart_preserves_agent_list() {}

/// Restart only the owned private tmux server; a reused pane ID with a foreign
/// tag must never be claimed, receive input, or be killed by the old agent.
#[test]
#[ignore = "covered by unit absent_dead_and_reused_panes_are_down_without_input; reused-pane restart acceptance not yet written"]
fn tmux_restart_does_not_claim_reused_pane() {}

/// Pending discovery is liveness, not a replacement session ID. A safe idle
/// restarted agent preserves its recorded binding and revalidates Ready.
#[test]
#[ignore = "covered by unit first_list_waits_for_initial_inspection_and_preserves_pending_binding; pending-row restart acceptance not yet written"]
fn idle_pending_restart_ready_preserves_binding() {}

/// A busy/unknown restart remains Starting until a later real event and safe
/// editor jointly permit Ready; stale idle history cannot satisfy it.
#[test]
#[ignore = "covered by unit busy_restart_recovers_on_event_and_new_real_row_binds; busy restart acceptance not yet written"]
fn busy_restart_starting_until_event() {}

/// A Ready/null process exits without End; a later positive idle-shell
/// observation records Down instead of trusting the retained harness frame.
#[test]
#[ignore = "covered by unit exit_to_shell_records_down_without_end; stand-in acceptance needs a full Ready/null exit-without-End fixture"]
fn ready_null_exit_without_end() {}

/// Exact canonical allowlist path permits No→Down→Yes→Enter once; confirm the
/// accepted editor/start and control.sent audit, never Enter on unselected No.
#[test]
#[ignore = "covered by unit claude_trust_no_down_yes_enter_editor; Claude folder-trust acceptance not yet written"]
fn claude_folder_trust_allowlist_audited() {}

/// An unallowlisted, prefix-only or changed folder goes Stuck with zero input;
/// compare with the allowlisted positive case, not merely a timeout.
#[test]
#[ignore = "covered by units unallowlisted_folder_is_stuck_with_zero_keys and trust_folder_must_equal_a_role_dir; unauthorized-folder acceptance not yet written"]
fn unallowlisted_folder_trust_zero_keys() {}

/// One trust-selection reset permits one recovery; a second reset is Stuck.
/// U11b currently models stable selection, so it cannot prove this sequence.
#[test]
#[ignore = "covered by units trust_reset_after_enter_recovers_with_second_select_and_accept and second_reset_after_enter_is_stuck; acceptance needs a trust-reset fixture"]
fn first_trust_reset_recovers_second_reset_stuck() {}

/// Codex update is skipped with exactly one guarded Escape; verify the
/// subsequent accepted state and control.sent, not merely key dispatch.
#[test]
#[ignore = "covered by units codex_update_escape_once_with_audit and update_prompt_still_shown_after_escape_is_stuck; update-dialog acceptance not yet written"]
fn update_skip_audited() {}

/// Default-selected Codex folder trust accepts once only for the exact allowed
/// path; verify readiness/audit and refusal of a changed selection or path.
#[test]
#[ignore = "covered by unit codex_folder_default_enter_with_audit; Codex folder-trust acceptance not yet written"]
fn codex_folder_trust_allowlist_audited() {}

/// While hook review is parked, the review modal is Stuck with an owner note
/// and no trust key. Deliberately replace this expectation when Task 9 lands.
#[test]
#[ignore = "covered by unit codex_folder_default_enter_with_audit (hook-review refusal); parked hook-review refusal acceptance not yet written"]
fn codex_hooks_review_is_stuck_without_literal_key() {}

/// Walk all hook-list pages, require the exact ATM event/count set, send the
/// approved literal trust key once, confirm accepted rows and audit evidence.
#[test]
#[ignore = "waits for Task 9: hook-review flow is parked"]
fn codex_exact_atm_hooks_only_audited() {}

/// Foreign, extra, changed, or hidden hook rows authorize no trust key; pair
/// with the exact-list success case, using controlled foreign-list evidence.
#[test]
#[ignore = "waits for Task 9 and a controlled foreign-hook-list fixture"]
fn codex_other_hooks_stuck() {}

/// Unknown/permission modal yields the specific refusal/Stuck and no input or
/// approval. Source replay can diagnose parsing, never prove a lifecycle turn.
#[test]
#[ignore = "covered by units permission_or_unknown_modal_sends_zero_keys and codex_warnings_modal_is_stuck; unknown-modal acceptance not yet written"]
fn unknown_modal_zero_keys() {}

/// Empty idle input with pi retry chrome is not Ready. No current U11b retry
/// scene exists; capture-backed tests elsewhere do not implement this case.
#[test]
#[ignore = "covered by unit pi_retry_empty_idle_not_ready; stand-in acceptance needs a pi-retry fixture"]
fn retry_chrome_empty_idle_blocks_ready() {}

/// Hold after clear consumption; watch clear_requested before raw session.send
/// and spawn_type (canonical, alias, %0N). Assert exact U8c refusal, then release
/// and await Ready. This does not prove message-delivery serialization.
#[test]
#[ignore = "agent.clear is in the base; clear-versus-raw-send stand-in acceptance not yet written"]
fn clear_vs_raw_send_refuses_until_ready() {}

/// With durable Clearing, restart the daemon and assert the same raw-send
/// refusals; do not assume the pre-restart clear RPC is still alive.
#[test]
#[ignore = "covered by unit reservation_durable_clearing_and_excluded_rows_survive_actor_restart; durable-clearing restart acceptance not yet written"]
fn durable_clearing_after_restart_refuses_raw_send() {}

/// Real msg.send stays queued behind held clear. Watch sent/clear_requested/
/// ready/delivered and the stand-in gates; exactly one new-generation prompt
/// contains the token, never concatenated /clear or /new. No substitute executor.
#[test]
fn msg_send_during_clear_delivers_once_to_new_generation() {
    let mode = StandInMode::Pi;
    let mut test = AgentRig::start_with_config(mode, DELIVERY);
    let config = support::started_config(mode);
    let started = Started::up(&test, "lead", &config);
    let (id, pane) = (started.id.as_str(), &started.pane);
    let old = test.complete_fixture_turn(pane);
    let cursor = test.cursor();
    let after = test.rig.event_cursor(pane);
    let call = test.call_in_background("agent.clear", json!({"agent": id, "timeout_ms": 30000}));
    test.rig.wait_event(pane, after, |e| {
        e["kind"] == "rendered" && e["state"] == "clearing"
    });
    // Sent while the stand-in holds the clear: recorded, and not typed.
    let sent = test.rpc(
        "msg.send",
        json!({"to": id, "body": "after the clear"}),
        None,
    );
    let message = sent["message_id"].as_str().unwrap().to_owned();
    let reference = format!("[{message}] Read: atm inbox --id {message}");
    assert!(!call.is_finished());
    assert_eq!(test.agent(id)["state"], "clearing");
    let held = test.rig.event_cursor(pane);
    let now = test.rig.control(pane, json!({"command":"barrier"}))["now_ms"]
        .as_u64()
        .unwrap();
    let reply = test.rig.control(
        pane,
        json!({"command":"advance_to", "now_ms": now + config.clear_gap_ms}),
    );
    assert!(reply["error"].is_null(), "{reply}");
    let cleared = call.join().unwrap().unwrap();
    let new = cleared["session_id"].as_str().unwrap().to_owned();
    assert_ne!(new, old.id.as_str());

    // The new session takes the reference as a prompt, exactly as sent.
    let accepted = test.rig.wait_event(pane, held, |e| e["kind"] == "accepted");
    assert_eq!(accepted["text"], reference.as_str(), "{accepted}");
    test.rig.wait("the delivery's record", || {
        test.tail(cursor)
            .iter()
            .any(|e| e["kind"] == "msg.delivered")
    });
    // One prompt since the clear began, in the new session: the clear
    // command was not a prompt, and the reference was not joined to it.
    let journal = test.rig.journal(pane);
    let since: Vec<_> = journal
        .iter()
        .filter(|e| e["event_sequence"].as_u64().unwrap() > after)
        .collect();
    let prompts: Vec<_> = since
        .iter()
        .filter(|e| matches!(e["kind"].as_str(), Some("accepted" | "rejected" | "queued")))
        .collect();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let inputs: Vec<_> = since.iter().filter(|e| e["event"] == "input").collect();
    assert_eq!(inputs.len(), 1, "{inputs:?}");
    assert_eq!(inputs[0]["data"]["session_id"], new.as_str());
    assert_eq!(inputs[0]["data"]["payload"]["text"], reference.as_str());

    // The record: the message was sent during the clear and attempted only
    // after the agent was bound to its new session; one attempt, delivered.
    let events = test.tail(cursor);
    let seq = |kind: &str| {
        let found: Vec<_> = events.iter().filter(|e| e["kind"] == kind).collect();
        assert_eq!(found.len(), 1, "{kind}: {events:?}");
        found[0]["seq"].as_u64().unwrap()
    };
    let order = [
        seq("agent.clear_requested"),
        seq("msg.sent"),
        seq("agent.session_bound"),
        seq("msg.delivery_attempted"),
        seq("msg.delivered"),
    ];
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{events:?}");
    let delivered = events
        .iter()
        .find(|e| e["kind"] == "msg.delivered")
        .unwrap();
    assert_eq!(
        delivered["body"],
        json!({
            "message_id": message,
            "session_id": new,
            "confirmation": "prompt_submit_exact_reference",
        })
    );
    let agent = test.agent(id);
    assert_eq!(agent["session_id"], new.as_str(), "{agent}");
    test.rig.close();
}

/// The `atm` binary itself, against the same private daemon: `atm agent up`,
/// `list`, `clear` and `down` print the daemon's answers. Up uses TMUX_PANE and
/// the rig's ATM_TMUX_SOCKET override; that override takes precedence over TMUX.
#[test]
fn atm_cli_starts_clears_and_takes_down_an_agent() {
    let mode = StandInMode::Pi;
    let mut test = AgentRig::start(mode);
    let config = support::started_config(mode);
    test.role("lead", mode, "");
    let log = test.next_stand_in("lead", &config);
    let placement = test.placement(&config);
    let atm = |args: &[&str]| {
        let mut command = test
            .rig
            .command(std::path::Path::new(env!("CARGO_BIN_EXE_atm")));
        command
            .args(args)
            .env("TMUX", test.rig.tmux_env())
            .env("TMUX_PANE", &placement);
        command
    };
    let json_of = |output: std::process::Output| -> serde_json::Value {
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let project = test.project.to_str().unwrap().to_owned();

    let up = json_of(test.rig.run(&mut atm(&[
        "agent",
        "up",
        "lead",
        "--project-root",
        &project,
        "--format",
        "json",
    ])));
    assert_eq!(up["agent_id"], "project/lead", "{up}");
    assert_eq!(up["state"], "ready", "{up}");
    let pane = test
        .rig
        .observe_pane(up["pane"].as_str().unwrap().to_owned(), mode, log);
    assert_eq!(test.tag(&pane.pane_id), "project/lead");
    let listed = json_of(test.rig.run(&mut atm(&[
        "agent",
        "list",
        "--project-root",
        &project,
        "--format",
        "json",
    ])));
    assert_eq!(listed[0]["agent_id"], "project/lead", "{listed}");
    assert_eq!(listed[0]["state"], "ready", "{listed}");
    assert_eq!(listed[0]["pane"], pane.pane_id);

    // The clear waits for the stand-in's clock, so the command runs while
    // the test moves it.
    let old = test.complete_fixture_turn(&pane);
    let after = test.rig.event_cursor(&pane);
    let clear = atm(&["agent", "clear", "project/lead", "--format", "json"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    test.rig.wait_event(&pane, after, |e| {
        e["kind"] == "rendered" && e["state"] == "clearing"
    });
    let now = test.rig.control(&pane, json!({"command":"barrier"}))["now_ms"]
        .as_u64()
        .unwrap();
    let reply = test.rig.control(
        &pane,
        json!({"command":"advance_to", "now_ms": now + config.clear_gap_ms}),
    );
    assert!(reply["error"].is_null(), "{reply}");
    let cleared = json_of(clear.wait_with_output().unwrap());
    let row = test.rig.wait_for_row(&pane, |r| {
        !r.id.is_pending() && r.id != old.id && r.status == SessionStatus::Idle
    });
    assert_eq!(
        cleared,
        json!({"agent_id": "project/lead", "session_id": row.id.as_str()})
    );

    let down = json_of(test.rig.run(&mut atm(&[
        "agent",
        "down",
        "project/lead",
        "--kill",
        "--format",
        "json",
    ])));
    assert_eq!(down, json!({"agent_id": "project/lead"}));
    stand_in::process::wait_exit(pane.pid, std::time::Duration::from_secs(30)).unwrap();
    let panes = checked(test.rig.tmux(&["list-panes", "-a", "-F", "#{pane_id}"]));
    assert!(!panes.lines().any(|p| p == pane.pane_id), "{panes}");
    let agent = test.agent("project/lead");
    assert_eq!(agent["state"], "down", "{agent}");
    test.rig.close();
}

/// A delivery policy whose waits are as short as the policy allows: the
/// tests wait on events, and the defaults (ten seconds of idle) would only
/// make them long.
const DELIVERY: &str = "[delivery]\nstable_idle = { secs = 0, nanos = 1000000 }\nstable_empty_input = { secs = 0, nanos = 1000000 }\n";

/// An already-admitted real delivery finishes before clear starts; later
/// delivery waits for readiness. The in-process coalesced-reservation callback
/// barrier belongs to U8c/B's separate reservation tests, not a new test RPC.
#[test]
#[ignore = "waits for Task 12: Task 8 plus U9b slice B integration"]
fn clear_waits_behind_admitted_delivery() {}
