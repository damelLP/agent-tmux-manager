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

// Acceptance inventory, NOT implemented assertions. Remove an ignore only
// together with its real test body; running these empty bodies proves nothing.
// Task numbers below are the lifecycle slice's task numbers.

/// Claude up: daemon ATM_SPAWN_BIN, role argv/model, ATM_AGENT, tag before first
/// start, real session plus safe editor, no implicit prompt, all in its own window.
#[test]
#[ignore = "waits for Task 6: agent.up acceptance"]
fn agent_up_ready_tagged_env_claude() {}

/// Codex up: daemon override, role/required args and ATM_AGENT, pre-start tag,
/// fresh safe Ready/null without submitting a prompt to manufacture a session.
#[test]
#[ignore = "waits for Task 6: agent.up acceptance"]
fn agent_up_ready_tagged_env_codex() {}

/// Pi up: daemon override, role args/model and ATM_AGENT, pre-start tag,
/// session_start plus safe editor before Ready, no implicit initial prompt.
#[test]
#[ignore = "waits for Task 6: agent.up acceptance"]
fn agent_up_ready_tagged_env_pi() {}

/// A linked checkout uses the main role, not a conflicting linked role. Scrub
/// all GIT_* for fixture git commands; prove the temp gitdir before any write.
#[test]
#[ignore = "waits for Task 6 and a scrubbed linked-checkout Git fixture"]
fn agent_up_reads_main_checkout_role() {}

/// Held Claude clear stays Clearing until a new real ID with source=clear and
/// a safe editor; return that ID on the same process, never rebind the old row.
#[test]
#[ignore = "waits for Task 8: Claude clear stand-in acceptance"]
fn clear_returns_new_ready_session_claude() {}

/// Held pi clear stays Clearing until changed ID plus safe editor on the same
/// process/connection; repeated notices alone are not acknowledgment.
#[test]
#[ignore = "waits for Task 8: pi clear stand-in acceptance"]
fn clear_returns_new_ready_session_pi() {}

/// Codex conversation-to-fresh clear returns Ready/null, excludes the old ID,
/// then binds the next real prompt's ID once without any slash concatenation.
#[test]
#[ignore = "waits for Task 8: Codex lazy-session clear acceptance"]
fn codex_clear_ready_without_id_then_binds_next_prompt() {}

/// Ready/null with no eligible row AND fresh safe facts sends no clear input
/// and emits no clear_requested; an unreported conversation must not pass.
#[test]
#[ignore = "waits for Task 8: Codex clear no-op acceptance"]
fn codex_ready_unbound_clear_is_noop() {}

/// Accept only the exact default current-worktree choice, create no checkout,
/// confirm a fresh safe editor and the specified control audit.
#[test]
#[ignore = "waits for Task 8: worktree clear acceptance"]
fn clear_worktree_default() {}

/// Respawn replaces pane/process/session under the same agent; stale old events
/// cannot bind. Use another pre-sized placement window, never split a stand-in.
#[test]
#[ignore = "waits for Tasks 6 and 10: respawn acceptance"]
fn respawn_uses_new_process_and_session() {}

/// A daemon restart retains durable identity/binding and converges after live
/// row and foreground-baseline revalidation, without lifecycle input.
#[test]
#[ignore = "waits for Task 10: restart foreground revalidation"]
fn restart_preserves_agent_list() {}

/// Restart only the owned private tmux server; a reused pane ID with a foreign
/// tag must never be claimed, receive input, or be killed by the old agent.
#[test]
#[ignore = "waits for Task 10: reused-pane restart acceptance"]
fn tmux_restart_does_not_claim_reused_pane() {}

/// Pending discovery is liveness, not a replacement session ID. A safe idle
/// restarted agent preserves its recorded binding and revalidates Ready.
#[test]
#[ignore = "waits for Task 10: pending-row restart acceptance"]
fn idle_pending_restart_ready_preserves_binding() {}

/// A busy/unknown restart remains Starting until a later real event and safe
/// editor jointly permit Ready; stale idle history cannot satisfy it.
#[test]
#[ignore = "waits for Task 10: busy restart acceptance"]
fn busy_restart_starting_until_event() {}

/// A Ready/null process exits without End; a later positive idle-shell
/// observation records Down instead of trusting the retained harness frame.
#[test]
#[ignore = "waits for Task 10 and a full Ready/null exit-without-End fixture"]
fn ready_null_exit_without_end() {}

/// Exact canonical allowlist path permits No→Down→Yes→Enter once; confirm the
/// accepted editor/start and control.sent audit, never Enter on unselected No.
#[test]
#[ignore = "waits for Tasks 6 and 7: Claude folder-trust acceptance"]
fn claude_folder_trust_allowlist_audited() {}

/// An unallowlisted, prefix-only or changed folder goes Stuck with zero input;
/// compare with the allowlisted positive case, not merely a timeout.
#[test]
#[ignore = "waits for Task 7: unauthorized-folder refusal acceptance"]
fn unallowlisted_folder_trust_zero_keys() {}

/// One trust-selection reset permits one recovery; a second reset is Stuck.
/// U11b currently models stable selection, so it cannot prove this sequence.
#[test]
#[ignore = "waits for Task 7 and the missing U11b trust-reset fixture"]
fn first_trust_reset_recovers_second_reset_stuck() {}

/// Codex update is skipped with exactly one guarded Escape; verify the
/// subsequent accepted state and control.sent, not merely key dispatch.
#[test]
#[ignore = "waits for Task 7: update-dialog acceptance"]
fn update_skip_audited() {}

/// Default-selected Codex folder trust accepts once only for the exact allowed
/// path; verify readiness/audit and refusal of a changed selection or path.
#[test]
#[ignore = "waits for Task 7: Codex folder-trust acceptance"]
fn codex_folder_trust_allowlist_audited() {}

/// While hook review is parked, the review modal is Stuck with an owner note
/// and no trust key. Deliberately replace this expectation when Task 9 lands.
#[test]
#[ignore = "waits for Task 7: parked hook-review refusal acceptance"]
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
#[ignore = "waits for Task 7: unknown-modal refusal acceptance"]
fn unknown_modal_zero_keys() {}

/// Empty idle input with pi retry chrome is not Ready. No current U11b retry
/// scene exists; capture-backed tests elsewhere do not implement this case.
#[test]
#[ignore = "waits for Tasks 6/8 and the missing U11b pi-retry fixture"]
fn retry_chrome_empty_idle_blocks_ready() {}

/// Hold after clear consumption; watch clear_requested before raw session.send
/// and spawn_type (canonical, alias, %0N). Assert exact U8c refusal, then release
/// and await Ready. This does not prove message-delivery serialization.
#[test]
#[ignore = "waits for Task 8: clear-versus-raw-send acceptance"]
fn clear_vs_raw_send_refuses_until_ready() {}

/// With durable Clearing, restart the daemon and assert the same raw-send
/// refusals; do not assume the pre-restart clear RPC is still alive.
#[test]
#[ignore = "waits for Tasks 8 and 10: durable-clearing restart acceptance"]
fn durable_clearing_after_restart_refuses_raw_send() {}

/// Real msg.send stays queued behind held clear. Watch sent/clear_requested/
/// ready/delivered and the stand-in gates; exactly one new-generation prompt
/// contains the token, never concatenated /clear or /new. No substitute executor.
#[test]
#[ignore = "waits for Task 12: Task 8 plus U9b slice B integration"]
fn msg_send_during_clear_delivers_once_to_new_generation() {}

/// An already-admitted real delivery finishes before clear starts; later
/// delivery waits for readiness. The in-process coalesced-reservation callback
/// barrier belongs to U8c/B's separate reservation tests, not a new test RPC.
#[test]
#[ignore = "waits for Task 12: Task 8 plus U9b slice B integration"]
fn clear_waits_behind_admitted_delivery() {}
