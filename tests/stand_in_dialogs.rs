//! Captured runtime screens and modal keys, synchronized on terminal replies.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/stand_in.rs"]
mod stand_in;

use stand_in::{StandInConfig, StandInMode, StandInPane, StandInRig};

fn frame(rig: &StandInRig, pane: &StandInPane, after: u64, name: &str) -> serde_json::Value {
    rig.wait_event(pane, after, |e| {
        e["kind"] == "rendered" && e["frame"] == name
    })
}

#[test]
fn native_fresh_screens_match_captures_and_exit_restores_shell() {
    let mut rig = StandInRig::start();
    for (mode, variant, fixture) in [
        (StandInMode::Claude, "rich-enabled", "crates/atm-claude-adapter/tests/fixtures/screen_u5b_claude-rich-enabled-fresh_2.1.325.txt"),
        (StandInMode::Codex, "rich", "crates/atm-codex-adapter/tests/fixtures/screen_u5b_codex-rich-fresh_0.167.0.txt"),
    ] {
        let mut config = StandInConfig::new(mode);
        config.screen_variant = variant.into();
        config.columns = 100;
        config.rows = 30;
        let pane = rig.launch(&config);
        let source = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture)).unwrap();
        let (_, expected) = source.split_once('\n').unwrap();
        assert_eq!(rig.capture_plain(&pane), expected);
        let after = rig.event_cursor(&pane);
        rig.paste(&pane, "/atm/exit");
        rig.wait_event(&pane, after, |e| e["kind"] == "exit");
        rig.wait_for_shell(&pane);
        assert!(!rig.capture_plain(&pane).contains("Claude Code"));
        assert!(!rig.capture_plain(&pane).contains("OpenAI Codex"));
        let journal = rig.journal(&pane);
        assert_eq!(journal.last().unwrap()["kind"], "exit");
    }
    rig.close();
}

#[test]
fn claude_trust_first_frame_has_no_ready_until_accepted() {
    let mut rig = StandInRig::start();
    let mut config = StandInConfig::new(StandInMode::Claude);
    config.folder_trust = true;
    config.folder = Some("/fixture/custom-folder".into());
    let pane = rig.launch(&config);
    frame(&rig, &pane, 0, "folder-trust");
    assert!(rig.capture_plain(&pane).contains("/fixture/custom-folder"));
    assert!(!rig
        .journal(&pane)
        .iter()
        .any(|e| e["kind"] == "ready" || e["event"] == "SessionStart"));
    assert!(rig.capture_plain(&pane).contains("❯ No, exit"));
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "modal text must not become a prompt");
    rig.key(&pane, "Down");
    rig.wait_event(&pane, after, |e| {
        e["kind"] == "rendered" && e["selection"] == 2
    });
    assert!(rig
        .capture_plain(&pane)
        .contains("❯ Yes, I trust this folder"));
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "Enter");
    frame(&rig, &pane, after, "fresh");
    rig.wait_ready(&pane, None);
    assert!(!rig
        .journal(&pane)
        .iter()
        .any(|e| e["event"] == "UserPromptSubmit"));
    rig.close();
}

#[test]
fn initial_cli_prompt_waits_for_startup_dialog_acceptance() {
    let mut rig = StandInRig::start();
    for mode in [StandInMode::Claude, StandInMode::Codex] {
        let log = rig.root().join(format!("{}-initial.jsonl", mode.name()));
        let config = log.with_extension("json");
        std::fs::write(
            &config,
            serde_json::json!({
                "LOG": log, "ID_PREFIX": "initial", "MANUAL_CLOCK": 1,
                "FOLDER_TRUST": 1,
            })
            .to_string(),
        )
        .unwrap();
        let output = rig.tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            rig.launcher(mode).to_str().unwrap(),
            "--atm-config",
            config.to_str().unwrap(),
            "initial prompt, not a dialog answer",
        ]);
        assert!(output.status.success(), "{output:?}");
        let pane = rig.observe_pane(
            String::from_utf8(output.stdout).unwrap().trim().into(),
            mode,
            log,
        );
        frame(&rig, &pane, 0, "folder-trust");
        assert!(!rig
            .journal(&pane)
            .iter()
            .any(|e| e["kind"] == "accepted" || e["kind"] == "exit"));
        let after = rig.event_cursor(&pane);
        if mode == StandInMode::Claude {
            rig.key(&pane, "Down");
        }
        rig.key(&pane, "Enter");
        frame(&rig, &pane, after, "working");
        let prompts: Vec<_> = rig
            .journal(&pane)
            .into_iter()
            .filter(|e| e["kind"] == "accepted")
            .collect();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0]["text"], "initial prompt, not a dialog answer");
        rig.wait_for_row(&pane, |row| row.status == atm_core::SessionStatus::Working);
    }
    rig.close();
}

#[test]
fn codex_start_dialogs_hook_list_and_worktree_default_are_real_frames() {
    let mut rig = StandInRig::start();
    let mut config = StandInConfig::new(StandInMode::Codex);
    config.manual_clock = true;
    config.update_prompt = true;
    config.folder_trust = true;
    config.hooks_review = true;
    config.worktree_choice = true;
    config.permission_dialog = true;
    config.columns = 100;
    config.rows = 32;
    let pane = rig.launch(&config);
    assert!(rig.capture_plain(&pane).contains("Update"));
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "Escape");
    frame(&rig, &pane, after, "folder-trust");
    assert!(rig.capture_plain(&pane).contains("Trust and continue"));
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "Enter");
    frame(&rig, &pane, after, "hooks-review");
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "Enter");
    frame(&rig, &pane, after, "hooks-list");
    let capture = rig.capture_plain(&pane);
    for event in ["PreToolUse", "SessionStart", "UserPromptSubmit"] {
        assert!(capture.contains(event), "{capture}");
    }
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "t");
    frame(&rig, &pane, after, "fresh");
    assert!(!rig
        .journal(&pane)
        .iter()
        .any(|e| e["event"] == "SessionStart"));
    let after = rig.event_cursor(&pane);
    rig.paste(&pane, "/new");
    frame(&rig, &pane, after, "worktree");
    assert!(rig
        .capture_plain(&pane)
        .contains("Use current Git worktree"));
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "Enter");
    frame(&rig, &pane, after, "clearing");
    let after = rig.event_cursor(&pane);
    rig.control(
        &pane,
        serde_json::json!({"command":"advance_to", "now_ms":100}),
    );
    frame(&rig, &pane, after, "clear-complete");
    let after = rig.event_cursor(&pane);
    rig.paste(&pane, "/atm/dialog");
    frame(&rig, &pane, after, "permission");
    assert!(rig
        .capture_plain(&pane)
        .contains("Would you like to run the following command?"));
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "not a prompt");
    rig.key(&pane, "Down");
    rig.key(&pane, "Enter");
    frame(&rig, &pane, after, "working"); // Option 2 is Yes, not denial.
    let after = rig.event_cursor(&pane);
    rig.control(
        &pane,
        serde_json::json!({"command":"advance_to", "now_ms":600}),
    );
    frame(&rig, &pane, after, "idle");
    assert!(rig
        .journal(&pane)
        .iter()
        .any(|e| e["event"] == "PostToolUse"));
    assert!(!rig
        .journal(&pane)
        .iter()
        .any(|e| e["event"] == "UserPromptSubmit"));
    rig.close();
}
