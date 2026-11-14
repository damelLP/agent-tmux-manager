use atm_codex_adapter::{
    screen::{self, observe_control_screen, validate_hook_list, CONTROL_POLICY},
    CodexEventType,
};
use atm_core::harness_control::{
    ClearProof, ControlDialog, ControlKey, HookListEntry, SessionArrival,
};
use atm_core::{styled_screen::StyledScreen, FreshEditorEvidence, ScreenInput, TurnState};
macro_rules! capture {
    ($name:literal) => {
        include_str!(concat!(
            "fixtures/screen_control_p1_",
            $name,
            "_0.167.0.txt"
        ))
    };
}
macro_rules! styled {
    ($name:literal) => {
        include_str!(concat!("fixtures/screen_styled_p1_", $name, "_0.167.0.txt"))
    };
}
fn body(s: &str) -> &str {
    if s.starts_with("Captured:") {
        s.split_once('\n').map_or(s, |(_, body)| body)
    } else {
        s
    }
}
fn observe(s: &str) -> atm_core::harness_control::ControlScreen {
    observe_control_screen(ScreenInput::Styled(body(s)), None)
}
fn plain(s: &str) -> String {
    StyledScreen::parse(body(s))
        .unwrap_or_else(|error| panic!("invalid fixture styles: {error}"))
        .text()
        .to_owned()
}
const PENDING: &str = capture!("a04-review-detail-100x32");
const TRUSTED: &str = capture!("a05-trust-t-100x32");
const FRESH: &str = styled!("b14-fresh-baseline-32-100x32");

#[test]
fn policy_reuses_existing_constants() {
    assert_eq!(CONTROL_POLICY.capture, screen::SCREEN_TEXT_FORMAT);
    assert_eq!(CONTROL_POLICY.clear_command, screen::CLEAR_COMMAND);
    assert_eq!(
        CONTROL_POLICY.session_arrival,
        SessionArrival::AtFirstPrompt
    );
    assert_eq!(
        CONTROL_POLICY.clear_proof,
        ClearProof::ConversationToFresh {
            ready_unbound_noop: true
        }
    );
    assert_eq!(CONTROL_POLICY.trust_selection_retries, 0);
}

#[test]
fn real_update_is_skippable_but_warnings_are_blocked() {
    // Captured: Codex 0.167.0, 2026-11-14, U5 private-tmux capture; sanitized.
    // U5 control-plane findings verified Escape skips this launch's update prompt;
    // P1 could not provoke it offline. No claim of a real styled update capture.
    for text in [
        include_str!("fixtures/screen_codex-startup_0.167.0.txt"),
        include_str!("fixtures/screen_codex-restart-startup_0.167.0.txt"),
    ] {
        assert_eq!(
            observe_control_screen(ScreenInput::Plain(text), None).dialog,
            ControlDialog::LaunchSkip {
                key: ControlKey::Escape
            }
        );
        // Synthetic: malformed options/selection/versions never become LaunchSkip.
        for damaged in [
            text.replace("› 1.", "1."),
            text.replace("Skip until next version", "Continue"),
            text.replace("enter continue", "enter trust"),
            text.replace("0.167.0", "bad"),
            text.replace("https://github.com", "https://elsewhere.invalid"),
        ] {
            assert_eq!(
                observe_control_screen(ScreenInput::Plain(&damaged), None).dialog,
                ControlDialog::Unknown
            );
        }
    }
    let warning = include_str!("fixtures/screen_codex-warning_0.167.0.txt");
    assert_eq!(
        observe_control_screen(ScreenInput::Plain(warning), None).dialog,
        ControlDialog::Blocked
    );
    assert_eq!(
        observe(styled!("c16-permission-visible-100x32")).dialog,
        ControlDialog::Unknown
    );
}

#[test]
fn captured_worktree_folder_and_hook_review_defaults() {
    for text in [
        capture!("b27-nodaemon-new-result-100x32"),
        capture!("b31-nodaemon-clear-dialog-100x32"),
        capture!("b35-default-repo-clear-dialog-100x24"),
        capture!("b38-poor-repeat-dialog-100x24"),
    ] {
        assert_eq!(
            observe(text).dialog,
            ControlDialog::CurrentWorktree {
                selected: true,
                accept: ControlKey::Enter
            }
        );
    }
    for (text, folder) in [
        (capture!("a14-nodaemon-later-100x32"), "repo"),
        (
            capture!("a16-default-fresh-folder-trust-100x24"),
            "repo-fresh",
        ),
    ] {
        assert_eq!(
            observe(text).dialog,
            ControlDialog::FolderTrust {
                folder: format!("/tmp/xxxxxxxxxxxxxxxxxxxxxx/{folder}"),
                selected_accept: true,
                select_accept: None,
                accept: ControlKey::Enter
            }
        );
    }
    for text in [
        capture!("a02-startup-100x32"),
        capture!("a10-corrected-hooks-review-100x32"),
        capture!("a17-default-fresh-hook-trust-100x24"),
    ] {
        assert_eq!(
            observe(text).dialog,
            ControlDialog::HookReview {
                default_selected: true,
                open: ControlKey::Enter
            }
        );
    }
}

#[test]
fn real_hook_pages_are_partial_never_complete() {
    for (text, review, active) in [
        (PENDING, Some(1), 0),
        (TRUSTED, None, 1),
        (capture!("a06-hooks-command-corrected-100x32"), None, 1),
    ] {
        let ControlDialog::HookList {
            entries,
            above,
            below,
            trust,
            close,
        } = observe(text).dialog
        else {
            panic!("not a hook list");
        };
        assert_eq!(entries.len(), 8);
        assert!(!above);
        assert!(below);
        assert_eq!(trust, ControlKey::Character('t'));
        assert_eq!(close, ControlKey::Escape);
        assert_eq!(entries.first().unwrap().event, "PreToolUse");
        assert_eq!(entries.last().unwrap().event, "UserPromptSubmit");
        assert!(entries
            .iter()
            .all(|e| e.installed == 1 && e.active == active && e.review == review));
        assert!(!validate_hook_list(&entries));
    }
}

#[test]
fn synthetic_complete_sets_follow_p6_exactly() {
    // Synthetic: no real complete/lower-page capture exists. These are validator
    // data, not invented accepted screens. Names come from the installed vocabulary.
    let pending: Vec<_> = CodexEventType::hook_names()
        .enumerate()
        .map(|(i, name)| {
            HookListEntry::new(name.into(), 1, (i % 2) as u32, Some(1 - (i % 2) as u32))
        })
        .collect();
    let trusted: Vec<_> = CodexEventType::hook_names()
        .map(|name| HookListEntry::new(name.into(), 1, 1, None))
        .collect();
    assert!(validate_hook_list(&pending));
    assert!(validate_hook_list(&trusted));
    let mut reordered = pending.clone();
    reordered.reverse();
    assert!(validate_hook_list(&reordered));
    assert!(!validate_hook_list(&[]));
    assert!(!validate_hook_list(&pending[..10]));
    for field in 0..7 {
        let mut rows = pending.clone();
        match field {
            0 => rows.push(rows[0].clone()),
            1 => rows[0].event = "Foreign".into(),
            2 => rows[0].installed = 2,
            3 => rows[0].active = 2,
            4 => rows[0].review = Some(2),
            5 => rows[0].review = None,
            _ => rows[0].installed = u32::MAX,
        }
        assert!(!validate_hook_list(&rows));
    }
    let mut inactive = trusted;
    inactive[0].active = 0;
    assert!(!validate_hook_list(&inactive));
}

#[test]
fn synthetic_malformed_dialogs_refuse_and_history_does_not_act() {
    for text in [
        capture!("a16-default-fresh-folder-trust-100x24"),
        capture!("a17-default-fresh-hook-trust-100x24"),
        capture!("b31-nodaemon-clear-dialog-100x32"),
    ] {
        let text = plain(text);
        for damaged in [
            text.replace("› 1.", "1."),
            text.replace("  2.", "› 2."),
            text.replace("enter ", "unknown "),
            format!("{text}\nunknown footer"),
            format!("{text}\x1b[8m"),
            format!("{text}\x1b["),
        ] {
            assert_eq!(observe(&damaged).dialog, ControlDialog::Unknown);
        }
        assert_eq!(
            observe(&format!("{text}\n{}", body(FRESH))).dialog,
            ControlDialog::None
        );
        assert_eq!(
            observe(&format!("{text}\nshell$ ")).dialog,
            ControlDialog::Unknown
        );
    }
    let folder = plain(capture!("a16-default-fresh-folder-trust-100x24"));
    for damaged in [
        folder.replace("/repo-fresh", "/..."),
        folder.replace("/repo-fresh", "/long\n wrapped"),
        folder.replace("Trust this folder?", "Trust?"),
    ] {
        assert_eq!(observe(&damaged).dialog, ControlDialog::Unknown);
    }
}

#[test]
fn synthetic_hook_table_damage_never_creates_complete_or_actionable_list() {
    let pending = plain(PENDING);
    let trusted = plain(TRUSTED);
    for damaged in [
        pending.replace("Review      Description", "Description"),
        pending.replace("t trust all", "t trust other"),
        pending.replace("need review before", "changed before"),
        pending.replace("PreToolUse", "Foreign"),
        pending.replace("PermissionRequest", "PreToolUse"),
        pending.replace("1           0           1", "2           0           1"),
        pending.replace("1           0           1", "1           2           1"),
        pending.replace("1           0           1", "4294967296  0           1"),
        pending.replace('↓', ""),
        pending.replace('↓', "↑"),
        trusted.replace("enter details", "enter review"),
        trusted.replace(
            "  Issues",
            "  ⚠ 11 hooks need review before they can run.\n  Issues",
        ),
        format!("{PENDING}\nunknown footer"),
    ] {
        assert_eq!(
            observe(&damaged).dialog,
            ControlDialog::Unknown,
            "{damaged}"
        );
    }
}

#[test]
fn conversation_evidence_does_not_change_worked_line_parser() {
    let completed = capture!("b12-completed-before-new-100x32");
    for text in [
        completed,
        styled!("c07-working-held-100x24"),
        styled!("c02-multiline-100x24"),
    ] {
        assert!(observe(text).conversation);
        assert_eq!(observe(text).dialog, ControlDialog::None);
        assert_eq!(
            observe(text).facts.fresh_editor,
            FreshEditorEvidence::Unknown
        );
    }
    assert_eq!(observe(completed).facts.turn, TurnState::Unknown); // U8g owns this.
    for text in [FRESH, styled!("b37-poor-baseline-100x24")] {
        assert!(!observe(text).conversation);
        assert_eq!(
            observe(text).facts.fresh_editor,
            FreshEditorEvidence::Present
        );
    }
    // Synthetic: composer text is not history; unknown active layout stays Unknown.
    let draft = plain(FRESH).replace("Ask Codex to do anything", "fake conversation");
    assert!(!observe(&draft).conversation);
    assert_eq!(
        observe(&format!("{FRESH}\nunknown footer")).dialog,
        ControlDialog::Unknown
    );
}
