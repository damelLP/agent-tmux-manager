//! Table tests for the pure flow decisions. No I/O.
//!
//! Screens are the adapters' real captures, read through the screen dispatch
//! (each adapter's fixture README records its `Captured:` line). Dialog values
//! built by hand are marked synthetic where they appear.

use super::agent_flow::*;
use super::agent_screen::{agent_screen, PaneBaseline};
use super::agent_screen_tests::{capture, pinned, ROOT};
use crate::control::reservation::{CaptureFormat, PaneKey};
use atm_core::harness_control::{ControlDialog, ControlKey, ControlPolicy, ControlScreen};
use atm_core::{Harness, SessionId};
use std::path::PathBuf;

macro_rules! claude {
    ($name:literal) => {
        include_str!(concat!(
            "../../../atm-claude-adapter/tests/fixtures/screen_control_p1_",
            $name,
            "_2.1.325.txt"
        ))
    };
}
macro_rules! codex {
    ($name:literal) => {
        include_str!(concat!(
            "../../../atm-codex-adapter/tests/fixtures/",
            $name,
            "_0.167.0.txt"
        ))
    };
}
macro_rules! pi {
    ($name:literal) => {
        include_str!(concat!(
            "../../../atm-pi-adapter/tests/fixtures/",
            $name,
            "_1.5.0.txt"
        ))
    };
}

const CLAUDE_FOLDER: &str = "/tmp/xxxxxxxxxxxxxxxxxxxxxxx/cwd";
const CODEX_FOLDER: &str = "/tmp/xxxxxxxxxxxxxxxxxxxxxx/repo-fresh";

fn name(harness: Harness) -> &'static str {
    match harness {
        Harness::ClaudeCode => "claude",
        Harness::Codex => "codex",
        _ => "pi",
    }
}

fn policy(harness: Harness) -> &'static ControlPolicy {
    agent_screen(harness).unwrap().policy()
}

/// The control screen the dispatch reads from a real capture with the pinned
/// harness in the foreground.
fn screen(harness: Harness, fixture: &str) -> ControlScreen {
    agent_screen(harness).unwrap().control(
        &capture(
            fixture,
            CaptureFormat::Ansi,
            Some(ROOT),
            Some(name(harness)),
        ),
        &pinned(name(harness)),
    )
}

fn dirs(folders: &[&str]) -> Vec<PathBuf> {
    folders.iter().map(PathBuf::from).collect()
}

fn session() -> SessionId {
    SessionId::new("new-session")
}

/// Run a start over the frames a flow would see, in order, as the flow does:
/// decide on each frame, and when the decision is a key, check that the guard
/// would pass on that same frame and count the key as sent. Stops at the first
/// Ready or Stuck.
fn drive(
    policy: &ControlPolicy,
    frames: &[&ControlScreen],
    trust_dirs: &[PathBuf],
    session: Option<&SessionId>,
) -> (Vec<Step>, Vec<String>) {
    let mut progress = StartProgress::default();
    let mut steps = Vec::new();
    let mut accepted = Vec::new();
    for frame in frames {
        accepted.extend(progress.trust_accepted(frame));
        let step = decide_start(policy, frame, &progress, trust_dirs, session);
        if let Step::Key { expect, action, .. } = &step {
            assert_eq!(
                *expect, frame.dialog,
                "a key is guarded on the frame it was decided on"
            );
            progress.sent(*action, expect);
        }
        let done = matches!(step, Step::Ready(_) | Step::Stuck(_));
        steps.push(step);
        if done {
            break;
        }
    }
    (steps, accepted)
}

fn keys(steps: &[Step]) -> Vec<(KeyAction, PaneKey)> {
    steps
        .iter()
        .filter_map(|step| match step {
            Step::Key { key, action, .. } => Some((*action, *key)),
            _ => None,
        })
        .collect()
}

fn stuck(step: Option<&Step>) -> &str {
    match step {
        Some(Step::Stuck(reason)) => reason,
        other => panic!("expected stuck, got {other:?}"),
    }
}

const SELECT: (KeyAction, PaneKey) = (KeyAction::TrustSelect, PaneKey::Down);
const ACCEPT: (KeyAction, PaneKey) = (KeyAction::TrustAccept, PaneKey::Enter);

/// Claude's trust frames at one width, in the order P1 captured them.
struct ClaudeTrust {
    no: ControlScreen,
    yes: ControlScreen,
    reset: ControlScreen,
    ready: ControlScreen,
}

fn claude_trust() -> Vec<ClaudeTrust> {
    let frames = |no, yes, reset, ready| ClaudeTrust {
        no: screen(Harness::ClaudeCode, no),
        yes: screen(Harness::ClaudeCode, yes),
        reset: screen(Harness::ClaudeCode, reset),
        ready: screen(Harness::ClaudeCode, ready),
    };
    vec![
        frames(
            claude!("A80-default-80x40"),
            claude!("A80-selected-80x40"),
            claude!("A80-reset-after-enter-80x40"),
            claude!("A80-ready-80x40"),
        ),
        frames(
            claude!("A120-default-120x40"),
            claude!("A120-selected-120x40"),
            claude!("A120-reset-after-enter-120x40"),
            claude!("A120-ready-120x40"),
        ),
        frames(
            claude!("A200-default-200x40"),
            claude!("A200-selected-200x40"),
            claude!("A200-reset-after-enter-200x40"),
            claude!("A200-ready-200x40"),
        ),
    ]
}

#[test]
fn the_real_frames_read_as_the_tables_assume() {
    for frames in claude_trust() {
        for (frame, selected) in [
            (&frames.no, false),
            (&frames.yes, true),
            (&frames.reset, false),
        ] {
            match &frame.dialog {
                ControlDialog::FolderTrust {
                    folder,
                    selected_accept,
                    select_accept,
                    accept,
                } => {
                    assert_eq!(folder, CLAUDE_FOLDER);
                    assert_eq!(*selected_accept, selected);
                    assert_eq!(*select_accept, (!selected).then_some(ControlKey::Down));
                    assert_eq!(*accept, ControlKey::Enter);
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(frames.ready.dialog, ControlDialog::None);
        // The reset frame is the default frame again, as far as the dialog goes.
        assert_eq!(frames.reset.dialog, frames.no.dialog);
    }
    for race in [
        claude!("A-race-0-yes-120x40"),
        claude!("A-race-1-yes-120x40"),
    ] {
        assert!(matches!(
            screen(Harness::ClaudeCode, race).dialog,
            ControlDialog::FolderTrust {
                selected_accept: true,
                ..
            }
        ));
    }
    assert_eq!(policy(Harness::ClaudeCode).trust_selection_retries, 1);
    assert_eq!(policy(Harness::Codex).trust_selection_retries, 0);
    assert_eq!(policy(Harness::Pi).trust_selection_retries, 0);
}

#[test]
fn start_table_claude() {
    let policy = policy(Harness::ClaudeCode);
    let trusted = dirs(&[CLAUDE_FOLDER]);
    for frames in claude_trust() {
        // No selected, Down, Yes selected, Enter, editor with its session.
        let (steps, accepted) = drive(
            policy,
            &[&frames.no, &frames.yes, &frames.ready],
            &trusted,
            Some(&session()),
        );
        assert_eq!(keys(&steps), [SELECT, ACCEPT]);
        assert_eq!(steps.last(), Some(&Step::Ready(Ready::Session(session()))));
        assert_eq!(accepted, [CLAUDE_FOLDER]);
        // The editor alone is not readiness: the session must have arrived.
        let (steps, _) = drive(policy, &[&frames.ready], &trusted, None);
        assert_eq!(
            steps,
            [Step::Wait("the harness has not announced its session")]
        );
        // A session without its editor (the turn of an initial prompt) waits.
        let running = screen(Harness::ClaudeCode, claude!("B-auto-submitted-120x40"));
        let (steps, _) = drive(policy, &[&running], &trusted, Some(&session()));
        assert_eq!(steps, [Step::Wait("the editor is not empty and idle")]);
    }
}

#[test]
fn never_enter_on_unselected_trust() {
    let policy = policy(Harness::ClaudeCode);
    let trusted = dirs(&[CLAUDE_FOLDER]);
    for frames in claude_trust() {
        // Whatever was sent before, a frame with "No" selected never gets Enter.
        for before in [
            vec![],
            vec![&frames.no],
            vec![&frames.no, &frames.yes],
            vec![&frames.no, &frames.yes, &frames.reset],
            vec![&frames.no, &frames.yes, &frames.reset, &frames.yes],
        ] {
            let mut sequence = before.clone();
            sequence.push(&frames.no);
            let (steps, _) = drive(policy, &sequence, &trusted, None);
            if steps.len() == sequence.len() {
                assert!(
                    !matches!(
                        steps.last(),
                        Some(Step::Key {
                            action: KeyAction::TrustAccept,
                            ..
                        })
                    ),
                    "{steps:?}"
                );
            }
        }
    }
}

#[test]
fn trust_reset_after_enter_recovers_with_second_select_and_accept() {
    let policy = policy(Harness::ClaudeCode);
    for frames in claude_trust() {
        let (steps, accepted) = drive(
            policy,
            &[
                &frames.no,
                &frames.yes,
                &frames.reset,
                &frames.yes,
                &frames.ready,
            ],
            &dirs(&[CLAUDE_FOLDER]),
            Some(&session()),
        );
        assert_eq!(keys(&steps), [SELECT, ACCEPT, SELECT, ACCEPT]);
        assert_eq!(steps.last(), Some(&Step::Ready(Ready::Session(session()))));
        assert_eq!(accepted, [CLAUDE_FOLDER], "audited once, on the editor");
    }
}

#[test]
fn accept_sent_dialog_unchanged_waits_without_second_enter() {
    let policy = policy(Harness::ClaudeCode);
    for frames in claude_trust() {
        // Enter was sent on Yes and the next captures still show Yes (no redraw yet).
        let (steps, accepted) = drive(
            policy,
            &[
                &frames.no,
                &frames.yes,
                &frames.yes,
                &frames.yes,
                &frames.ready,
            ],
            &dirs(&[CLAUDE_FOLDER]),
            Some(&session()),
        );
        assert_eq!(keys(&steps), [SELECT, ACCEPT]);
        assert_eq!(
            steps[2..4],
            [
                Step::Wait("the trust dialog is still shown after it was accepted"),
                Step::Wait("the trust dialog is still shown after it was accepted"),
            ]
        );
        assert_eq!(steps.last(), Some(&Step::Ready(Ready::Session(session()))));
        assert_eq!(accepted, [CLAUDE_FOLDER]);
    }
    // The race frames P1 captured are that state too.
    let frames = &claude_trust()[1];
    let race = screen(Harness::ClaudeCode, claude!("A-race-0-yes-120x40"));
    let (steps, _) = drive(
        policy,
        &[&frames.no, &frames.yes, &race],
        &dirs(&[CLAUDE_FOLDER]),
        None,
    );
    assert_eq!(keys(&steps), [SELECT, ACCEPT]);
    assert!(matches!(steps.last(), Some(Step::Wait(_))));
}

#[test]
fn second_reset_after_enter_is_stuck() {
    let policy = policy(Harness::ClaudeCode);
    for frames in claude_trust() {
        let (steps, accepted) = drive(
            policy,
            &[
                &frames.no,
                &frames.yes,
                &frames.reset,
                &frames.yes,
                &frames.reset,
                &frames.yes,
            ],
            &dirs(&[CLAUDE_FOLDER]),
            Some(&session()),
        );
        assert_eq!(keys(&steps), [SELECT, ACCEPT, SELECT, ACCEPT]);
        assert_eq!(steps.len(), 5, "nothing is decided after stuck");
        assert_eq!(
            stuck(steps.last()),
            "the trust dialog reset again after it was accepted"
        );
        assert!(accepted.is_empty());
    }
}

#[test]
fn select_sent_still_unselected_waits_without_second_select() {
    let policy = policy(Harness::ClaudeCode);
    for frames in claude_trust() {
        let (steps, _) = drive(
            policy,
            &[
                &frames.no,
                &frames.no,
                &frames.no,
                &frames.yes,
                &frames.ready,
            ],
            &dirs(&[CLAUDE_FOLDER]),
            Some(&session()),
        );
        // One select, however long "No" stays on screen; then the usual accept.
        assert_eq!(keys(&steps), [SELECT, ACCEPT]);
        assert_eq!(
            steps[1],
            Step::Wait("the trust dialog does not show the accepting option selected yet")
        );
        assert_eq!(steps.last(), Some(&Step::Ready(Ready::Session(session()))));
    }
}

#[test]
fn trust_folder_must_equal_a_role_dir() {
    let policy = policy(Harness::ClaudeCode);
    let frames = &claude_trust()[1];
    for (case, trusted) in [
        ("no directories", dirs(&[])),
        ("a parent", dirs(&["/tmp/xxxxxxxxxxxxxxxxxxxxxxx"])),
        ("a child", dirs(&["/tmp/xxxxxxxxxxxxxxxxxxxxxxx/cwd/sub"])),
        ("a prefix", dirs(&["/tmp/xxxxxxxxxxxxxxxxxxxxxxx/cw"])),
        (
            "a trailing slash",
            dirs(&["/tmp/xxxxxxxxxxxxxxxxxxxxxxx/cwd/"]),
        ),
        ("a home-relative form", dirs(&["~/cwd"])),
        ("another directory", dirs(&["/srv/elsewhere"])),
    ] {
        for frame in [&frames.no, &frames.yes] {
            let (steps, _) = drive(policy, &[frame], &trusted, None);
            assert!(keys(&steps).is_empty(), "{case}");
            assert!(
                stuck(steps.last()).contains("not one of the role's trust directories"),
                "{case}"
            );
        }
    }
    // The twin: the exact directory among others is answered.
    let (steps, _) = drive(
        policy,
        &[&frames.no],
        &dirs(&["/srv/elsewhere", CLAUDE_FOLDER]),
        None,
    );
    assert_eq!(keys(&steps), [SELECT]);
}

#[test]
fn trust_folder_change_is_stuck() {
    // Synthetic: the same real dialog asking about a second trusted folder.
    let policy = policy(Harness::ClaudeCode);
    let frames = &claude_trust()[1];
    let mut other = frames.yes.clone();
    if let ControlDialog::FolderTrust { folder, .. } = &mut other.dialog {
        *folder = "/srv/elsewhere".into();
    }
    let trusted = dirs(&[CLAUDE_FOLDER, "/srv/elsewhere"]);
    let (steps, _) = drive(policy, &[&frames.no, &other], &trusted, None);
    assert_eq!(keys(&steps), [SELECT]);
    assert!(stuck(steps.last()).contains("changed its folder"));
    // The twin: on its own, that folder's dialog is accepted.
    let (steps, _) = drive(policy, &[&other], &trusted, None);
    assert_eq!(keys(&steps), [ACCEPT]);
}

#[test]
fn trust_dialog_gone_after_only_a_select_is_stuck() {
    let policy = policy(Harness::ClaudeCode);
    let frames = &claude_trust()[1];
    let (steps, accepted) = drive(
        policy,
        &[&frames.no, &frames.ready],
        &dirs(&[CLAUDE_FOLDER]),
        Some(&session()),
    );
    assert_eq!(keys(&steps), [SELECT]);
    assert_eq!(
        stuck(steps.last()),
        "the trust dialog went away without being accepted by atm"
    );
    assert!(accepted.is_empty());
}

#[test]
fn start_table_codex() {
    let policy = policy(Harness::Codex);
    let folder = screen(
        Harness::Codex,
        codex!("screen_control_p1_a16-default-fresh-folder-trust-100x24"),
    );
    let hooks = screen(
        Harness::Codex,
        codex!("screen_control_p1_a17-default-fresh-hook-trust-100x24"),
    );
    let fresh = screen(Harness::Codex, codex!("screen_styled_u5b_rich-fresh"));
    let conversation = screen(
        Harness::Codex,
        codex!("screen_styled_p1_c11-working-released-100x24"),
    );
    let working = screen(
        Harness::Codex,
        codex!("screen_styled_p1_c07-working-held-100x24"),
    );
    let trusted = dirs(&[CODEX_FOLDER]);

    // codex_default_selected_folder_accepts_once: accept is selected by default,
    // so there is no select, one Enter, and no second round (retries 0).
    let (steps, accepted) = drive(policy, &[&folder, &folder, &fresh], &trusted, None);
    assert_eq!(keys(&steps), [ACCEPT]);
    assert_eq!(steps.last(), Some(&Step::Ready(Ready::Editor)));
    assert_eq!(accepted, [CODEX_FOLDER]);

    // Readiness is the fresh editor alone; no session is waited for or used.
    for session in [None, Some(session())] {
        let (steps, _) = drive(policy, &[&fresh], &trusted, session.as_ref());
        assert_eq!(steps, [Step::Ready(Ready::Editor)]);
    }
    let (steps, _) = drive(policy, &[&working], &trusted, None);
    assert_eq!(steps, [Step::Wait("the editor is not empty and idle")]);
    // A conversation in a harness that was just launched is not a fresh editor.
    assert!(conversation.conversation);
    let (steps, _) = drive(policy, &[&conversation], &trusted, None);
    assert!(stuck(steps.last()).contains("a conversation is on screen"));

    // An unallowlisted folder: zero keys.
    let (steps, _) = drive(policy, &[&folder], &dirs(&["/srv/elsewhere"]), None);
    assert!(keys(&steps).is_empty());
    assert!(stuck(steps.last()).contains(CODEX_FOLDER));

    // hooks_need_review_is_stuck_without_literal_key
    let (steps, _) = drive(policy, &[&folder, &hooks], &trusted, None);
    assert_eq!(keys(&steps), [ACCEPT]);
    assert!(stuck(steps.last()).contains("review its hooks"));
}

#[test]
fn codex_trust_reset_has_no_second_round() {
    // Synthetic: Codex's real dialog with its other option selected, as a reset
    // after Enter would show it. Its policy allows no retry.
    let policy = policy(Harness::Codex);
    let folder = screen(
        Harness::Codex,
        codex!("screen_control_p1_a16-default-fresh-folder-trust-100x24"),
    );
    let mut reset = folder.clone();
    if let ControlDialog::FolderTrust {
        selected_accept,
        select_accept,
        ..
    } = &mut reset.dialog
    {
        *selected_accept = false;
        *select_accept = Some(ControlKey::Up);
    }
    let (steps, _) = drive(policy, &[&folder, &reset], &dirs(&[CODEX_FOLDER]), None);
    assert_eq!(keys(&steps), [ACCEPT]);
    assert_eq!(
        stuck(steps.last()),
        "the trust dialog reset again after it was accepted"
    );
    // Not selected and no key to select it: nothing is sent at all.
    if let ControlDialog::FolderTrust { select_accept, .. } = &mut reset.dialog {
        *select_accept = None;
    }
    let (steps, _) = drive(policy, &[&reset], &dirs(&[CODEX_FOLDER]), None);
    assert!(keys(&steps).is_empty());
    assert!(stuck(steps.last()).contains("offers no key to select it"));
}

/// Codex's update prompt and Warnings modal exist only as plain captures
/// (U5); they carry no styles, so they are read here as the styled capture
/// they would be without any. Not a real styled capture of either.
#[test]
fn update_prompt_is_skipped_once() {
    let policy = policy(Harness::Codex);
    let update = screen(Harness::Codex, codex!("screen_codex-startup"));
    let fresh = screen(Harness::Codex, codex!("screen_styled_u5b_rich-fresh"));
    assert_eq!(
        update.dialog,
        ControlDialog::LaunchSkip {
            key: ControlKey::Escape
        }
    );
    let (steps, accepted) = drive(policy, &[&update, &fresh], &[], None);
    assert_eq!(
        keys(&steps),
        [(KeyAction::SkipLaunchPrompt, PaneKey::Escape)]
    );
    assert_eq!(steps.last(), Some(&Step::Ready(Ready::Editor)));
    assert!(accepted.is_empty());
}

#[test]
fn update_prompt_still_shown_after_escape_is_stuck() {
    // No second Escape: the flow waits, and its deadline makes this the reason.
    let policy = policy(Harness::Codex);
    let update = screen(Harness::Codex, codex!("screen_codex-startup"));
    let (steps, _) = drive(policy, &[&update, &update, &update], &[], None);
    assert_eq!(
        keys(&steps),
        [(KeyAction::SkipLaunchPrompt, PaneKey::Escape)]
    );
    assert_eq!(
        steps[1..],
        [
            Step::Wait("the update prompt is still shown after it was skipped"),
            Step::Wait("the update prompt is still shown after it was skipped"),
        ]
    );
}

#[test]
fn unknown_screen_waits_blocked_screen_is_stuck() {
    let codex = policy(Harness::Codex);
    let warnings = screen(Harness::Codex, codex!("screen_codex-warning"));
    assert_eq!(warnings.dialog, ControlDialog::Blocked);
    let (steps, _) = drive(codex, &[&warnings], &[], None);
    assert!(keys(&steps).is_empty());
    assert_eq!(stuck(steps.last()), "a dialog atm must not answer is open");

    // Screens no adapter answers for: a permission prompt, a selector, a shell.
    for (harness, fixture) in [
        (
            Harness::Codex,
            codex!("screen_styled_p1_c16-permission-visible-100x32"),
        ),
        (Harness::ClaudeCode, claude!("C3-dialog-120x40")),
        (Harness::Pi, pi!("screen_styled_p1_C3-one-selector-120x40")),
        (Harness::Pi, "$ pi\n"),
    ] {
        let unknown = screen(harness, fixture);
        assert_eq!(unknown.dialog, ControlDialog::Unknown, "{harness:?}");
        let (steps, _) = drive(
            policy(harness),
            &[&unknown, &unknown],
            &[],
            Some(&session()),
        );
        assert_eq!(
            steps,
            [
                Step::Wait("the screen was not recognised"),
                Step::Wait("the screen was not recognised"),
            ]
        );
    }
    // Synthetic: dialogs that belong to other flows, and keys that have no route.
    let mut other = ControlScreen::default();
    other.dialog = ControlDialog::CurrentWorktree {
        selected: true,
        accept: ControlKey::Enter,
    };
    let (steps, _) = drive(codex, &[&other], &[], None);
    assert!(keys(&steps).is_empty());
    assert!(stuck(steps.last()).contains("unexpected dialog"));
    other.dialog = ControlDialog::LaunchSkip {
        key: ControlKey::Character('s'),
    };
    let (steps, _) = drive(codex, &[&other], &[], None);
    assert!(keys(&steps).is_empty());
    assert!(stuck(steps.last()).contains("a key atm cannot send"));
    assert_eq!(pane_key(ControlKey::Character('t')), None);
}

#[test]
fn start_table_pi() {
    let policy = policy(Harness::Pi);
    let ready = screen(Harness::Pi, pi!("screen_styled_p1_A-ready-120x40"));
    let working = screen(
        Harness::Pi,
        pi!("screen_styled_p1_C2-steer-one-held-120x40"),
    );
    let (steps, accepted) = drive(policy, &[&ready], &[], Some(&session()));
    assert_eq!(steps, [Step::Ready(Ready::Session(session()))]);
    assert!(accepted.is_empty());
    let (steps, _) = drive(policy, &[&ready], &[], None);
    assert_eq!(
        steps,
        [Step::Wait("the harness has not announced its session")]
    );
    let (steps, _) = drive(policy, &[&working], &[], Some(&session()));
    assert_eq!(steps, [Step::Wait("the editor is not empty and idle")]);
}

#[test]
fn at_most_one_plus_retries_of_each_key() {
    // Every sequence of eight observations over the trust table's states.
    let frames = &claude_trust()[1];
    let unknown = ControlScreen::default();
    let alphabet = [&frames.no, &frames.yes, &unknown, &frames.ready];
    for (harness, retries) in [(Harness::ClaudeCode, 1usize), (Harness::Codex, 0)] {
        let policy = policy(harness);
        assert_eq!(usize::from(policy.trust_selection_retries), retries);
        for code in 0..4usize.pow(8) {
            let sequence: Vec<&ControlScreen> = (0..8)
                .map(|position| alphabet[(code >> (2 * position)) & 3])
                .collect();
            let (steps, accepted) = drive(policy, &sequence, &dirs(&[CLAUDE_FOLDER]), None);
            let keys = keys(&steps);
            let selects = keys.iter().filter(|key| **key == SELECT).count();
            let accepts = keys.iter().filter(|key| **key == ACCEPT).count();
            assert_eq!(selects + accepts, keys.len());
            assert!(selects <= 1 + retries && accepts <= 1 + retries, "{keys:?}");
            // An acceptance is reported at most once per accept that was sent.
            assert!(accepted.len() <= accepts, "{keys:?} {accepted:?}");
            // Enter only on a frame that shows Yes selected; Down only on No.
            for (step, frame) in steps.iter().zip(&sequence) {
                match step {
                    Step::Key {
                        action: KeyAction::TrustAccept,
                        ..
                    } => {
                        assert_eq!(frame.dialog, frames.yes.dialog);
                    }
                    Step::Key { .. } => assert_eq!(frame.dialog, frames.no.dialog),
                    _ => {}
                }
            }
            // Two keys of one kind never follow each other without the other.
            assert!(keys.windows(2).all(|pair| pair[0] != pair[1]), "{keys:?}");
        }
    }
}

// --- the start phase: which program is in front (section 3.2) ---

fn shell() -> PaneBaseline {
    PaneBaseline::shell(ROOT, "zsh".into())
}

#[test]
fn shell_right_after_launch_enter_waits() {
    // The launch line is still the last thing on screen and the shell is in front.
    for harness in [Harness::ClaudeCode, Harness::Codex, Harness::Pi] {
        let dispatch = agent_screen(harness).unwrap();
        for text in [
            "% cd '/proj' && ATM_AGENT='p/lead' claude\n",
            claude!("A120-default-120x40"),
        ] {
            assert_eq!(
                start_view(
                    dispatch,
                    &capture(text, CaptureFormat::Ansi, Some(ROOT), Some("zsh")),
                    &shell()
                ),
                StartView::Wait("the launch has started no harness"),
                "{harness:?}"
            );
        }
    }
}

#[test]
fn shell_after_pin_is_stuck_not_down() {
    // Claude was killed while its trust dialog showed; the stale frame still
    // parses, but the shell is in front: no screen is read, nothing is sent.
    let dispatch = agent_screen(Harness::ClaudeCode).unwrap();
    let stale = capture(
        claude!("A120-selected-120x40"),
        CaptureFormat::Ansi,
        Some(ROOT),
        Some("zsh"),
    );
    assert_eq!(
        start_view(dispatch, &stale, &pinned("claude")),
        StartView::Stuck("the harness exited to its shell during the start".into())
    );
    // The twin: the same frame with Claude in front is its dialog.
    let live = capture(
        claude!("A120-selected-120x40"),
        CaptureFormat::Ansi,
        Some(ROOT),
        Some("claude"),
    );
    assert!(matches!(
        start_view(dispatch, &live, &pinned("claude")),
        StartView::Harness(ControlScreen {
            dialog: ControlDialog::FolderTrust { .. },
            ..
        })
    ));
}

#[test]
fn harness_command_pinned_at_first_recognised_screen() {
    // A start dialog is a recognised screen: the pin comes before readiness.
    for (harness, fixture) in [
        (Harness::ClaudeCode, claude!("A120-default-120x40")),
        (
            Harness::Codex,
            codex!("screen_control_p1_a16-default-fresh-folder-trust-100x24"),
        ),
        (Harness::Pi, pi!("screen_styled_p1_A-ready-120x40")),
    ] {
        let dispatch = agent_screen(harness).unwrap();
        let frame = capture(
            fixture,
            CaptureFormat::Ansi,
            Some(ROOT),
            Some(name(harness)),
        );
        match start_view(dispatch, &frame, &shell()) {
            StartView::Pin { baseline, control } => {
                assert_eq!(baseline.harness_command(), Some(name(harness)));
                assert_eq!(control, screen(harness, fixture));
                // From here the same capture is the pinned harness's screen.
                assert_eq!(
                    start_view(dispatch, &frame, &baseline),
                    StartView::Harness(control)
                );
            }
            other => panic!("{harness:?}: {other:?}"),
        }
    }
}

#[test]
fn node_frames_pin_nothing_then_harness_name_pins() {
    // P4: an npm launch shows `node` first, then the harness's own name. The
    // `node` captures show the shell's launch line, which no adapter recognises.
    for (harness, fixture) in [
        (Harness::Codex, codex!("screen_styled_u5b_rich-fresh")),
        (Harness::Pi, pi!("screen_styled_p1_A-ready-120x40")),
    ] {
        let dispatch = agent_screen(harness).unwrap();
        let launching = capture("% codex\n", CaptureFormat::Ansi, Some(ROOT), Some("node"));
        assert_eq!(
            start_view(dispatch, &launching, &shell()),
            StartView::Wait("no harness screen was recognised")
        );
        let ready = capture(
            fixture,
            CaptureFormat::Ansi,
            Some(ROOT),
            Some(name(harness)),
        );
        assert!(matches!(
            start_view(dispatch, &ready, &shell()),
            StartView::Pin { baseline, .. } if baseline.harness_command() == Some(name(harness))
        ));
    }
}

#[test]
fn foreground_name_change_after_pin_is_stuck() {
    // Unverified in a real harness: a recognised frame drawn while the name is
    // still `node`. Then `node` is pinned and `pi` is another program: nothing
    // is read or sent, and the wait's reason is what the deadline reports.
    let dispatch = agent_screen(Harness::Pi).unwrap();
    let early = capture(
        pi!("screen_styled_p1_A-ready-120x40"),
        CaptureFormat::Ansi,
        Some(ROOT),
        Some("node"),
    );
    let StartView::Pin { baseline, .. } = start_view(dispatch, &early, &shell()) else {
        panic!("a recognised screen pins the command in front");
    };
    assert_eq!(baseline.harness_command(), Some("node"));
    let renamed = capture(
        pi!("screen_styled_p1_A-ready-120x40"),
        CaptureFormat::Ansi,
        Some(ROOT),
        Some("pi"),
    );
    assert_eq!(
        start_view(dispatch, &renamed, &baseline),
        StartView::Wait("the pane's foreground command changed after the harness started")
    );
}

#[test]
fn start_view_refuses_what_it_cannot_place() {
    let dispatch = agent_screen(Harness::Pi).unwrap();
    let fixture = pi!("screen_styled_p1_A-ready-120x40");
    for (root, command) in [
        (None, Some("pi")),
        (Some(ROOT + 1), Some("pi")),
        (Some(ROOT), None),
    ] {
        for baseline in [shell(), pinned("pi")] {
            let view = start_view(
                dispatch,
                &capture(fixture, CaptureFormat::Ansi, root, command),
                &baseline,
            );
            assert!(
                matches!(view, StartView::Wait(_)),
                "{root:?} {command:?}: {view:?}"
            );
        }
    }
    // A shell-named harness can't be pinned: it reads as the shell and waits.
    assert_eq!(
        start_view(
            dispatch,
            &capture(fixture, CaptureFormat::Ansi, Some(ROOT), Some("zsh")),
            &shell()
        ),
        StartView::Wait("the launch has started no harness")
    );
    // The wrong capture format is never a recognised screen.
    assert_eq!(
        start_view(
            dispatch,
            &capture(fixture, CaptureFormat::Plain, Some(ROOT), Some("pi")),
            &shell()
        ),
        StartView::Wait("the pane's processes could not be read")
    );
}

// --- the launch line (section 3.4) ---

/// One staged launch line as tmux showed it, from the rows around the prompt.
struct Staged {
    shell: &'static str,
    tmux: &'static str,
    case: &'static str,
    width: u16,
    /// (column, row) of the cursor, the row counted within `rows`.
    cursor: (u32, u32),
    /// The row the prompt and the pasted text start on.
    prompt_row: usize,
    rows: &'static [&'static str],
    text: &'static str,
}

// Captured: tmux 3.8 (bash 5.3, zsh 5.9, bash as sh) and tmux 3.6 (bash, dash
// as sh), 2026-11-14, private-server probe: a launch line pasted at the prompt
// `pb> ` of a 49-column pane with `paste-buffer -p`, read with
// `capture-pane -p -N` before any Enter. Six rows around the line; run
// directories replaced by same-length placeholders. The probe's other cases
// did not keep the prompt's row and are not used.
const STAGED: &[Staged] = &[
    Staged {
        shell: "bash",
        tmux: "tmux 3.8",
        case: "staged_wrapped",
        width: 49,
        cursor: (46, 4),
        prompt_row: 1,
        rows: &["pb> ^C      ", "pb> cd '/tmp/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "xxxx/cwd dir' && ATM_AGENT='p/lead' pbharness '/t", "mp/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/h' 'n", "ever-run' --model 'example model' --flag 'a;b'   ", ""],
        text: "cd '/tmp/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/cwd dir' && ATM_AGENT='p/lead' pbharness '/tmp/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/h' 'never-run' --model 'example model' --flag 'a;b'",
    },
    Staged {
        shell: "zsh",
        tmux: "tmux 3.8",
        case: "staged_wrapped",
        width: 49,
        cursor: (44, 4),
        prompt_row: 1,
        rows: &["pb>         ", "pb> cd '/tmp/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "xxx/cwd dir' && ATM_AGENT='p/lead' pbharness '/tm", "p/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/h' 'nev", "er-run' --model 'example model' --flag 'a;b'     ", ""],
        text: "cd '/tmp/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/cwd dir' && ATM_AGENT='p/lead' pbharness '/tmp/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/h' 'never-run' --model 'example model' --flag 'a;b'",
    },
    Staged {
        shell: "sh",
        tmux: "tmux 3.8",
        case: "staged_wrapped",
        width: 49,
        cursor: (42, 4),
        prompt_row: 1,
        rows: &["pb> ^C      ", "pb> cd '/tmp/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "xx/cwd dir' && ATM_AGENT='p/lead' pbharness '/tmp", "/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/h' 'never", "-run' --model 'example model' --flag 'a;b'       ", ""],
        text: "cd '/tmp/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/cwd dir' && ATM_AGENT='p/lead' pbharness '/tmp/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/h' 'never-run' --model 'example model' --flag 'a;b'",
    },
    Staged {
        shell: "bash",
        tmux: "tmux 3.6",
        case: "staged_wrapped",
        width: 49,
        cursor: (47, 4),
        prompt_row: 2,
        rows: &["PB-BACK-alias           ", "pb> ^C      ", "pb> cd '/tmp/yyyyyyyyyyyyyyyy/cwd dir' && ATM_AGE", "NT='p/lead' pbharness '/tmp/yyyyyyyyyyyyyyyy/h' '", "never-run' --model 'example model' --flag 'a;b'  ", ""],
        text: "cd '/tmp/yyyyyyyyyyyyyyyy/cwd dir' && ATM_AGENT='p/lead' pbharness '/tmp/yyyyyyyyyyyyyyyy/h' 'never-run' --model 'example model' --flag 'a;b'",
    },
    Staged {
        shell: "bash",
        tmux: "tmux 3.6",
        case: "staged_exact_fit",
        width: 49,
        cursor: (0, 4),
        prompt_row: 0,
        rows: &["pb> cd '/tmp/yyyyyyyyyyyyyyyy/cwd dir' && ATM_AGE", "NT='p/lead' pbharness '/tmp/yyyyyyyyyyyyyyyy/h' '", "never-run' --model 'example model' --flag 'a;b' -", "-pad 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'", "            ", ""],
        text: "cd '/tmp/yyyyyyyyyyyyyyyy/cwd dir' && ATM_AGENT='p/lead' pbharness '/tmp/yyyyyyyyyyyyyyyy/h' 'never-run' --model 'example model' --flag 'a;b' --pad 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'",
    },
    Staged {
        shell: "bash",
        tmux: "tmux 3.6",
        case: "staged_exact_fit_plus_one",
        width: 49,
        cursor: (1, 4),
        prompt_row: 0,
        rows: &["pb> cd '/tmp/yyyyyyyyyyyyyyyy/cwd dir' && ATM_AGE", "NT='p/lead' pbharness '/tmp/yyyyyyyyyyyyyyyy/h' '", "never-run' --model 'example model' --flag 'a;b' -", "-pad 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "'           ", ""],
        text: "cd '/tmp/yyyyyyyyyyyyyyyy/cwd dir' && ATM_AGENT='p/lead' pbharness '/tmp/yyyyyyyyyyyyyyyy/h' 'never-run' --model 'example model' --flag 'a;b' --pad 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'",
    },
    Staged {
        shell: "sh",
        tmux: "tmux 3.6",
        case: "staged_wrapped",
        width: 49,
        cursor: (43, 4),
        prompt_row: 2,
        rows: &["PB-BACK-alias           ", "pb> ^C      ", "pb> cd '/tmp/yyyyyyyyyyyyyy/cwd dir' && ATM_AGENT", "='p/lead' pbharness '/tmp/yyyyyyyyyyyyyy/h' 'neve", "r-run' --model 'example model' --flag 'a;b'      ", ""],
        text: "cd '/tmp/yyyyyyyyyyyyyy/cwd dir' && ATM_AGENT='p/lead' pbharness '/tmp/yyyyyyyyyyyyyy/h' 'never-run' --model 'example model' --flag 'a;b'",
    },
    Staged {
        shell: "sh",
        tmux: "tmux 3.6",
        case: "staged_exact_fit",
        width: 49,
        cursor: (49, 4),
        prompt_row: 1,
        rows: &["r-run' --model 'example model' --flag 'a;b'^C    ", "pb> cd '/tmp/yyyyyyyyyyyyyy/cwd dir' && ATM_AGENT", "='p/lead' pbharness '/tmp/yyyyyyyyyyyyyy/h' 'neve", "r-run' --model 'example model' --flag 'a;b' --pad", " 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'", ""],
        text: "cd '/tmp/yyyyyyyyyyyyyy/cwd dir' && ATM_AGENT='p/lead' pbharness '/tmp/yyyyyyyyyyyyyy/h' 'never-run' --model 'example model' --flag 'a;b' --pad 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'",
    },
    Staged {
        shell: "sh",
        tmux: "tmux 3.6",
        case: "staged_exact_fit_plus_one",
        width: 49,
        cursor: (1, 4),
        prompt_row: 0,
        rows: &["pb> cd '/tmp/yyyyyyyyyyyyyy/cwd dir' && ATM_AGENT", "='p/lead' pbharness '/tmp/yyyyyyyyyyyyyy/h' 'neve", "r-run' --model 'example model' --flag 'a;b' --pad", " 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "'           ", ""],
        text: "cd '/tmp/yyyyyyyyyyyyyy/cwd dir' && ATM_AGENT='p/lead' pbharness '/tmp/yyyyyyyyyyyyyy/h' 'never-run' --model 'example model' --flag 'a;b' --pad 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'",
    },
];

fn shell_capture(
    rows: &[&str],
    width: u16,
    cursor: (u32, u32),
    format: CaptureFormat,
) -> crate::control::reservation::PaneCapture {
    let mut text = rows.join("\n");
    text.push('\n');
    let inspection = crate::control::reservation::PaneInspection::new(
        atm_tmux::PaneInfo {
            pane_id: "%1".into(),
            session_name: "private".into(),
            window_index: 0,
            pane_pid: ROOT,
            width,
            height: rows.len() as u16,
            is_active: false,
        },
        Some("p/lead".into()),
        false,
        false,
        Some(true),
        cursor.0,
        cursor.1,
    )
    .with_foreground_process(None, Some("zsh".into()));
    crate::control::reservation::PaneCapture::new(text, inspection, format)
}

impl Staged {
    fn after(&self, format: CaptureFormat) -> crate::control::reservation::PaneCapture {
        shell_capture(self.rows, self.width, self.cursor, format)
    }
    /// The pane as it was before the paste: the same rows down to the prompt,
    /// with the cursor after the prompt.
    fn before(&self) -> crate::control::reservation::PaneCapture {
        let mut rows = self.rows[..self.prompt_row].to_vec();
        rows.push("pb> ");
        rows.resize(self.rows.len(), "");
        shell_capture(
            &rows,
            self.width,
            (4, self.prompt_row as u32),
            CaptureFormat::Plain,
        )
    }
    fn label(&self) -> String {
        format!("{} on {}, {}", self.shell, self.tmux, self.case)
    }
}

#[test]
fn launch_line_equals_prompt_plus_text() {
    let mut exact_fits = 0;
    for staged in STAGED {
        assert!(launch_text_is_safe(staged.text), "{}", staged.label());
        let before = launch_line(&staged.before()).unwrap();
        assert!(before.ends_with("pb> "), "{}", staged.label());
        for format in [CaptureFormat::Plain, CaptureFormat::Ansi] {
            let after = staged.after(format);
            assert_eq!(
                launch_line(&after),
                Some(format!("{before}{}", staged.text)),
                "{}",
                staged.label()
            );
            assert!(launch_line_is_staged(&after, &before, staged.text));
        }
        exact_fits +=
            usize::from(staged.cursor.0 == 0 || staged.cursor.0 == u32::from(staged.width));
    }
    assert_eq!(STAGED.len(), 9);
    assert!(
        exact_fits >= 2,
        "both exact-fit cursor shapes are among the captures"
    );
}

#[test]
fn launch_line_cursor_at_width_is_row_end() {
    // dash after an exact fit: the cursor stays on the full row, at column 49.
    let staged = STAGED
        .iter()
        .find(|staged| staged.cursor.0 == u32::from(staged.width))
        .expect("the dash exact-fit capture");
    assert_eq!(staged.shell, "sh");
    let line = launch_line(&staged.after(CaptureFormat::Plain)).unwrap();
    assert!(line.ends_with(staged.text));
    // bash after the same fit: column 0 of the next row, the full row joined in.
    let staged = STAGED
        .iter()
        .find(|staged| staged.cursor.0 == 0)
        .expect("the bash exact-fit capture");
    let line = launch_line(&staged.after(CaptureFormat::Plain)).unwrap();
    assert!(line.ends_with(staged.text));
    // Synthetic: a column past the width is not a cursor.
    let mut capture = staged.after(CaptureFormat::Plain);
    capture.inspection.cursor_column = u32::from(staged.width) + 1;
    assert_eq!(launch_line(&capture), None);
}

#[test]
fn launch_line_ignores_cells_at_or_after_cursor() {
    // The exact-fit-plus-one captures keep eleven stale cells after the cursor.
    let staged = STAGED
        .iter()
        .find(|staged| staged.case == "staged_exact_fit_plus_one")
        .unwrap();
    let cursor_row = staged.rows[staged.cursor.1 as usize];
    assert!(
        cursor_row.len() > staged.cursor.0 as usize,
        "{cursor_row:?}"
    );
    let before = launch_line(&staged.before()).unwrap();
    assert!(launch_line_is_staged(
        &staged.after(CaptureFormat::Plain),
        &before,
        staged.text
    ));
    // Synthetic: a shell's suggestion drawn after the cursor changes nothing.
    let mut rows = staged.rows.to_vec();
    let suggested = format!("{} --resume last", &cursor_row[..staged.cursor.0 as usize]);
    rows[staged.cursor.1 as usize] = &suggested;
    let capture = shell_capture(&rows, staged.width, staged.cursor, CaptureFormat::Plain);
    assert!(launch_line_is_staged(&capture, &before, staged.text));
}

#[test]
fn launch_line_with_typed_prefix_refuses() {
    // Synthetic: the review's scenario. Someone typed `rm -rf ~; ` into the new
    // shell before the paste landed. The line ends with the launch text, but it
    // is not the prompt plus the launch text.
    let before = shell_capture(&["pb> ", ""], 80, (4, 0), CaptureFormat::Plain);
    let p0 = launch_line(&before).unwrap();
    assert_eq!(p0, "pb> ");
    let text = "cd '/proj' && ATM_AGENT='p/lead' claude";
    let typed = format!("pb> rm -rf ~; {text}");
    let capture = shell_capture(
        &[&typed, ""],
        80,
        (typed.len() as u32, 0),
        CaptureFormat::Plain,
    );
    assert!(launch_line(&capture).unwrap().ends_with(text));
    assert!(!launch_line_is_staged(&capture, &p0, text));
    // The twin: the same pane without the typed text.
    let clean = format!("pb> {text}");
    let capture = shell_capture(
        &[&clean, ""],
        80,
        (clean.len() as u32, 0),
        CaptureFormat::Plain,
    );
    assert!(launch_line_is_staged(&capture, &p0, text));
    // Text typed after the paste is before the cursor too.
    let after = format!("pb> {text} --dangerously");
    let capture = shell_capture(
        &[&after, ""],
        80,
        (after.len() as u32, 0),
        CaptureFormat::Plain,
    );
    assert!(!launch_line_is_staged(&capture, &p0, text));
}

#[test]
fn launch_line_is_staged_when_the_text_ends_in_a_space() {
    // A launch text can end in a space (a whitespace-only `ARGS` override).
    // Synthetic: a capture shows that space only as the cursor's column, with
    // or without the cell written.
    let text = "cd '/proj' && ATM_AGENT='p/lead' claude ' ' ";
    for row in [format!("pb> {text}"), format!("pb> {}", text.trim_end())] {
        let end = 4 + text.len() as u32;
        let capture = shell_capture(&[&row, ""], 80, (end, 0), CaptureFormat::Plain);
        assert!(launch_line_is_staged(&capture, "pb> ", text), "{row:?}");
        // The twin: the cursor one cell short of the text's end.
        let capture = shell_capture(&[&row, ""], 80, (end - 1, 0), CaptureFormat::Plain);
        assert!(!launch_line_is_staged(&capture, "pb> ", text), "{row:?}");
    }
}

#[test]
fn launch_line_with_changed_prompt_refuses() {
    // Synthetic: an asynchronous prompt redraw between the paste and the Enter.
    let text = "cd '/proj' && ATM_AGENT='p/lead' claude";
    let p0 = "pb> ";
    for prompt in ["pb (main)> ", "pb>", "> ", ""] {
        let line = format!("{prompt}{text}");
        let capture = shell_capture(
            &[&line, ""],
            80,
            (line.len() as u32, 0),
            CaptureFormat::Plain,
        );
        assert!(!launch_line_is_staged(&capture, p0, text), "{prompt:?}");
    }
    // The cursor moved back into the text: the line before it is shorter.
    let line = format!("{p0}{text}");
    let capture = shell_capture(
        &[&line, ""],
        80,
        (line.len() as u32 - 3, 0),
        CaptureFormat::Plain,
    );
    assert!(!launch_line_is_staged(&capture, p0, text));
    // The cursor sits after cells that were never written: they read as blanks.
    let capture = shell_capture(&["pb>", ""], 80, (6, 0), CaptureFormat::Plain);
    assert_eq!(launch_line(&capture).as_deref(), Some("pb>   "));
}

#[test]
fn launch_line_measures_prompt_glyphs_by_cells() {
    // Probe P-B2 (tmux 3.8 and 3.6; bash, zsh, dash): the column tmux reports
    // for the cursor after each of these prompts. The last is the left prompt
    // of an installed powerlevel10k. A capture holds one character per glyph,
    // however many cells it takes.
    let text = "cd '/proj' && ATM_AGENT='p/lead' claude";
    for (prompt, column) in [
        ("\u{276f} ", 2),
        ("\u{754c} ", 3),
        ("\u{1f600} ", 3),
        ("\u{e0b0} ", 2),
        ("\u{f303} \u{f015} ~ \u{276f} ", 8),
    ] {
        let before = shell_capture(&[prompt, ""], 80, (column, 0), CaptureFormat::Plain);
        assert_eq!(launch_line(&before).as_deref(), Some(prompt), "{prompt:?}");
        let row = format!("{prompt}{text}");
        let end = column + text.len() as u32;
        let after = shell_capture(&[&row, ""], 80, (end, 0), CaptureFormat::Plain);
        assert!(launch_line_is_staged(&after, prompt, text), "{prompt:?}");
        // The twin: the same prompt with text typed ahead of the paste.
        let row = format!("{prompt}x{text}");
        let after = shell_capture(&[&row, ""], 80, (end + 1, 0), CaptureFormat::Plain);
        assert!(!launch_line_is_staged(&after, prompt, text), "{prompt:?}");
    }
    // Cells, not characters: three cells are the wide glyph and its space,
    // and the character after them is at the cursor, not before it.
    let capture = shell_capture(&["\u{754c} x", ""], 80, (3, 0), CaptureFormat::Plain);
    assert_eq!(launch_line(&capture).as_deref(), Some("\u{754c} "));
    // Synthetic: a right-hand prompt fills the row. It is after the cursor.
    let prompt = "\u{f303} \u{f015} ~ \u{276f} ";
    let row = format!("{prompt}{:>72}", "user@host ");
    let capture = shell_capture(&[&row, ""], 80, (8, 0), CaptureFormat::Plain);
    assert_eq!(launch_line(&capture).as_deref(), Some(prompt));
    // A full row of wide glyphs above the cursor's row is a wrapped line.
    let wide = "\u{1f680}".repeat(40);
    let capture = shell_capture(&[&wide, "pb> "], 80, (4, 1), CaptureFormat::Plain);
    assert_eq!(launch_line(&capture), Some(format!("{wide}pb> ")));
}

#[test]
fn launch_line_refuses_what_it_cannot_measure() {
    // Synthetic. tmux's cursor column falls inside a wide character: its
    // widths and ours disagree about this row, so nothing is read from it.
    let capture = shell_capture(&["\u{754c} ", ""], 80, (1, 0), CaptureFormat::Plain);
    assert_eq!(launch_line(&capture), None);
    // The twin: the column after the character.
    let capture = shell_capture(&["\u{754c} ", ""], 80, (2, 0), CaptureFormat::Plain);
    assert_eq!(launch_line(&capture).as_deref(), Some("\u{754c}"));
    // The row holds more cells than the pane is wide: the same disagreement.
    let wide = "\u{754c}".repeat(41);
    let capture = shell_capture(&[&wide, ""], 80, (4, 0), CaptureFormat::Plain);
    assert_eq!(launch_line(&capture), None);
    let wide = "\u{754c}".repeat(40);
    let capture = shell_capture(&[&wide, ""], 80, (4, 0), CaptureFormat::Plain);
    assert_eq!(launch_line(&capture).as_deref(), Some("\u{754c}\u{754c}"));
    // A character without a certain width before the cursor: a combining
    // mark, a joiner, a variation selector, a control character.
    for prompt in [
        "cafe\u{301}> ",
        "a\u{200d}b> ",
        "\u{2764}\u{fe0f} ",
        "pb\t> ",
    ] {
        let capture = shell_capture(&[prompt, ""], 80, (6, 0), CaptureFormat::Plain);
        assert_eq!(launch_line(&capture), None, "{prompt:?}");
        // After the cursor it is not read.
        let row = format!("pb> {prompt}");
        let capture = shell_capture(&[&row, ""], 80, (4, 0), CaptureFormat::Plain);
        assert_eq!(launch_line(&capture).as_deref(), Some("pb> "), "{prompt:?}");
    }
    // A row above that may or may not fill the width: unknown, so refuse.
    // (78 cells for certain, 80 if the mark takes two.)
    let unsure = format!("{}e\u{301}", "x".repeat(77));
    let capture = shell_capture(&[&unsure, "pb> "], 80, (4, 1), CaptureFormat::Plain);
    assert_eq!(launch_line(&capture), None);
    // The twins: a row above too short to fill the width under any widths,
    // and one that is wider than the pane.
    let capture = shell_capture(&["e\u{301} done", "pb> "], 80, (4, 1), CaptureFormat::Plain);
    assert_eq!(launch_line(&capture).as_deref(), Some("pb> "));
    let wide = "\u{754c}".repeat(41);
    let capture = shell_capture(&[&wide, "pb> "], 80, (4, 1), CaptureFormat::Plain);
    assert_eq!(launch_line(&capture), None);
    // An undecodable styled capture and a cursor below the capture.
    let capture = shell_capture(&["pb> \u{1b}[5n", ""], 80, (4, 0), CaptureFormat::Ansi);
    assert_eq!(launch_line(&capture), None);
    let mut capture = shell_capture(&["pb> ", ""], 80, (4, 0), CaptureFormat::Plain);
    capture.inspection.cursor_row = 2;
    assert_eq!(launch_line(&capture), None);
    // Styles are not cells: a coloured prompt reads as its text.
    let capture = shell_capture(
        &["\u{1b}[1;32mpb>\u{1b}[0m ", ""],
        80,
        (4, 0),
        CaptureFormat::Ansi,
    );
    assert_eq!(launch_line(&capture).as_deref(), Some("pb> "));
}

#[test]
fn launch_text_refuses_non_ascii() {
    for text in [
        "cd '/proj' && ATM_AGENT='p/lead' claude --model 'opus'",
        "x",
        "a b~!",
    ] {
        assert!(launch_text_is_safe(text), "{text:?}");
    }
    for text in [
        "",
        "cd '/home/caf\u{e9}' && claude",
        "claude --name '\u{1f680}'",
        "claude\n",
        "claude\trm",
        "claude\u{1b}[31m",
        "claude\u{7f}",
        "claude\u{a0}--x",
    ] {
        assert!(!launch_text_is_safe(text), "{text:?}");
    }
}

// --- the pane's process shape (section 3.2, the procfs read) ---

// Captured: tmux 3.8 (bash 5.3, zsh 5.9, bash as sh) and tmux 3.6 (bash, dash
// as sh), 2026-11-14, private-server probe with a blocking stand-in program.
// Per state: the root's process group and its terminal's foreground group
// (both relative to the root's pid), the root's child count and `argv[0]`,
// whether the root is a shell, tmux's `pane_current_command`, and the shape
// the probe's findings assign. The probe checked the shells against
// `/etc/shells` by hand; it did not record the root's executable.
type Measured = (
    &'static str,
    i32,
    i32,
    usize,
    &'static str,
    bool,
    &'static str,
    PaneShape,
);
const MEASURED: &[Measured] = &[
    (
        "bash on tmux 3.8: fresh_split",
        0,
        0,
        0,
        "-bash",
        true,
        "bash",
        PaneShape::IdleShell,
    ),
    (
        "bash on tmux 3.8: job_control_on_running",
        0,
        116,
        1,
        "-bash",
        true,
        "python3",
        PaneShape::JobInFront,
    ),
    (
        "bash on tmux 3.8: job_control_on_after_exit",
        0,
        0,
        0,
        "-bash",
        true,
        "bash",
        PaneShape::IdleShell,
    ),
    (
        "bash on tmux 3.8: job_control_off_running",
        0,
        0,
        1,
        "-bash",
        true,
        "bash",
        PaneShape::ShellHoldingChild,
    ),
    (
        "bash on tmux 3.8: job_control_off_after_exit",
        0,
        0,
        0,
        "-bash",
        true,
        "bash",
        PaneShape::IdleShell,
    ),
    (
        "bash on tmux 3.8: command_then_exec_shell_running",
        0,
        0,
        1,
        "bash",
        true,
        "bash",
        PaneShape::ShellHoldingChild,
    ),
    (
        "bash on tmux 3.8: command_then_exec_shell_after_exit",
        0,
        0,
        0,
        "/bin/bash",
        true,
        "bash",
        PaneShape::IdleShell,
    ),
    (
        "bash on tmux 3.8: harness_as_pane_command_running",
        0,
        0,
        0,
        "/usr/bin/python3",
        false,
        "python3",
        PaneShape::RootProgram,
    ),
    (
        "zsh on tmux 3.8: fresh_split",
        0,
        0,
        0,
        "-zsh",
        true,
        "zsh",
        PaneShape::IdleShell,
    ),
    (
        "zsh on tmux 3.8: job_control_on_running",
        0,
        61,
        1,
        "-zsh",
        true,
        "python3",
        PaneShape::JobInFront,
    ),
    (
        "zsh on tmux 3.8: job_control_on_after_exit",
        0,
        0,
        0,
        "-zsh",
        true,
        "zsh",
        PaneShape::IdleShell,
    ),
    (
        "zsh on tmux 3.8: job_control_off_running",
        0,
        0,
        1,
        "-zsh",
        true,
        "zsh",
        PaneShape::ShellHoldingChild,
    ),
    (
        "zsh on tmux 3.8: job_control_off_after_exit",
        0,
        0,
        0,
        "-zsh",
        true,
        "zsh",
        PaneShape::IdleShell,
    ),
    (
        "zsh on tmux 3.8: command_then_exec_shell_running",
        0,
        0,
        1,
        "zsh",
        true,
        "zsh",
        PaneShape::ShellHoldingChild,
    ),
    (
        "zsh on tmux 3.8: command_then_exec_shell_after_exit",
        0,
        0,
        0,
        "/bin/zsh",
        true,
        "zsh",
        PaneShape::IdleShell,
    ),
    (
        "zsh on tmux 3.8: harness_as_pane_command_running",
        0,
        0,
        0,
        "/usr/bin/python3",
        false,
        "python3",
        PaneShape::RootProgram,
    ),
    (
        "sh on tmux 3.8: fresh_split",
        0,
        0,
        0,
        "-sh",
        true,
        "sh",
        PaneShape::IdleShell,
    ),
    (
        "sh on tmux 3.8: job_control_on_running",
        0,
        101,
        1,
        "-sh",
        true,
        "python3",
        PaneShape::JobInFront,
    ),
    (
        "sh on tmux 3.8: job_control_on_after_exit",
        0,
        0,
        0,
        "-sh",
        true,
        "sh",
        PaneShape::IdleShell,
    ),
    (
        "sh on tmux 3.8: job_control_off_running",
        0,
        0,
        1,
        "-sh",
        true,
        "sh",
        PaneShape::ShellHoldingChild,
    ),
    (
        "sh on tmux 3.8: job_control_off_after_exit",
        0,
        0,
        0,
        "-sh",
        true,
        "sh",
        PaneShape::IdleShell,
    ),
    (
        "sh on tmux 3.8: command_then_exec_shell_running",
        0,
        0,
        1,
        "sh",
        true,
        "sh",
        PaneShape::ShellHoldingChild,
    ),
    (
        "sh on tmux 3.8: command_then_exec_shell_after_exit",
        0,
        0,
        0,
        "/bin/sh",
        true,
        "sh",
        PaneShape::IdleShell,
    ),
    (
        "sh on tmux 3.8: harness_as_pane_command_running",
        0,
        0,
        0,
        "/usr/bin/python3",
        false,
        "python3",
        PaneShape::RootProgram,
    ),
    (
        "bash on tmux 3.6: fresh_split",
        0,
        0,
        0,
        "-bash",
        true,
        "bash",
        PaneShape::IdleShell,
    ),
    (
        "bash on tmux 3.6: job_control_on_running",
        0,
        46,
        1,
        "-bash",
        true,
        "python3",
        PaneShape::JobInFront,
    ),
    (
        "bash on tmux 3.6: job_control_on_after_exit",
        0,
        0,
        0,
        "-bash",
        true,
        "bash",
        PaneShape::IdleShell,
    ),
    (
        "bash on tmux 3.6: job_control_off_running",
        0,
        0,
        1,
        "-bash",
        true,
        "bash",
        PaneShape::ShellHoldingChild,
    ),
    (
        "bash on tmux 3.6: job_control_off_after_exit",
        0,
        0,
        0,
        "-bash",
        true,
        "bash",
        PaneShape::IdleShell,
    ),
    (
        "bash on tmux 3.6: command_then_exec_shell_running",
        0,
        0,
        1,
        "bash",
        true,
        "bash",
        PaneShape::ShellHoldingChild,
    ),
    (
        "bash on tmux 3.6: command_then_exec_shell_after_exit",
        0,
        0,
        0,
        "/bin/bash",
        true,
        "bash",
        PaneShape::IdleShell,
    ),
    (
        "bash on tmux 3.6: harness_as_pane_command_running",
        0,
        0,
        0,
        "/usr/bin/python3",
        false,
        "python3",
        PaneShape::RootProgram,
    ),
    (
        "sh on tmux 3.6: fresh_split",
        0,
        0,
        0,
        "-sh",
        true,
        "sh",
        PaneShape::IdleShell,
    ),
    (
        "sh on tmux 3.6: job_control_on_running",
        0,
        47,
        1,
        "-sh",
        true,
        "python3",
        PaneShape::JobInFront,
    ),
    (
        "sh on tmux 3.6: job_control_on_after_exit",
        0,
        0,
        0,
        "-sh",
        true,
        "sh",
        PaneShape::IdleShell,
    ),
    (
        "sh on tmux 3.6: job_control_off_running",
        0,
        0,
        1,
        "-sh",
        true,
        "sh",
        PaneShape::ShellHoldingChild,
    ),
    (
        "sh on tmux 3.6: job_control_off_after_exit",
        0,
        0,
        0,
        "-sh",
        true,
        "sh",
        PaneShape::IdleShell,
    ),
    (
        "sh on tmux 3.6: command_then_exec_shell_running",
        0,
        0,
        1,
        "sh",
        true,
        "sh",
        PaneShape::ShellHoldingChild,
    ),
    (
        "sh on tmux 3.6: command_then_exec_shell_after_exit",
        0,
        0,
        0,
        "/bin/sh",
        true,
        "sh",
        PaneShape::IdleShell,
    ),
    (
        "sh on tmux 3.6: harness_as_pane_command_running",
        0,
        0,
        1,
        "sh",
        true,
        "sh",
        PaneShape::ShellHoldingChild,
    ),
];

const SHELL_EXE: &str = "/usr/bin/a-shell";

fn facts(
    group: i32,
    foreground: i32,
    children: usize,
    argv0: &str,
    shell: bool,
) -> Option<ProcFacts> {
    let pid = 5000;
    let stat = format!(
        "{pid} (comm) S 1 {} {} 34816 {} 4194304 100 0 0 0",
        pid + group,
        pid + group,
        pid + foreground
    );
    let children: Vec<String> = (0..children)
        .map(|child| (pid + 100 + child as i32).to_string())
        .collect();
    ProcFacts::parse(
        &stat,
        format!("{argv0}\0--flag\0").as_bytes(),
        &children.join(" "),
        std::path::Path::new(if shell { SHELL_EXE } else { "/usr/bin/python3" }),
        &[
            PathBuf::from("/usr/bin/other-shell"),
            PathBuf::from(SHELL_EXE),
        ],
    )
}

#[test]
fn pane_shape_table() {
    let mut seen = std::collections::BTreeMap::new();
    for (label, group, foreground, children, argv0, shell, command, expected) in MEASURED {
        let facts = facts(*group, *foreground, *children, argv0, *shell).unwrap();
        assert_eq!(classify_pane(&facts), Some(*expected), "{label}");
        *seen.entry(format!("{expected:?}")).or_insert(0) += 1;
        // tmux names an idle shell as the root's own name.
        if *expected == PaneShape::IdleShell {
            assert_eq!(*command, facts.name, "{label}");
        }
    }
    assert_eq!(MEASURED.len(), 40);
    assert_eq!(
        seen.into_iter().collect::<Vec<_>>(),
        [
            ("IdleShell".to_owned(), 20),
            ("JobInFront".to_owned(), 5),
            ("RootProgram".to_owned(), 4),
            ("ShellHoldingChild".to_owned(), 11),
        ]
    );
    // Synthetic: a terminal with no foreground group is none of the four.
    let mut detached = facts(0, 0, 0, "-zsh", true).unwrap();
    detached.foreground_group = -1;
    assert_eq!(classify_pane(&detached), None);
    detached.foreground_group = 0;
    assert_eq!(classify_pane(&detached), None);
}

#[test]
fn baseline_table() {
    for (label, group, foreground, children, argv0, shell, command, shape) in MEASURED {
        let facts = facts(*group, *foreground, *children, argv0, *shell).unwrap();
        let launch = shell_baseline(&facts, ROOT, command);
        let adopt = adopt_baseline(&facts, ROOT, command);
        match shape {
            // A launch may type into an idle shell; there is nothing to adopt.
            PaneShape::IdleShell => {
                assert_eq!(
                    launch,
                    Some(PaneBaseline::shell(ROOT, (*command).into())),
                    "{label}"
                );
                assert_eq!(adopt, None, "{label}");
            }
            // A job in front, or the program as the pane's root: adoptable, and
            // never a shell to type a launch into.
            PaneShape::JobInFront | PaneShape::RootProgram => {
                assert_eq!(launch, None, "{label}");
                assert_eq!(adopt, Some(PaneBaseline::adopted(ROOT)), "{label}");
            }
            // Job control off, `child; exec shell`, dash's `sh -c`: the shell's
            // name is shown while the child runs. Nothing is recorded.
            PaneShape::ShellHoldingChild => {
                assert_eq!(launch, None, "{label}");
                assert_eq!(adopt, None, "{label}");
            }
        }
    }
    // Synthetic: a foreground job that runs under its shell's own name (a
    // wrapper script that does not exec) can't be told from the shell.
    let wrapper = facts(0, 50, 1, "-bash", true).unwrap();
    assert_eq!(classify_pane(&wrapper), Some(PaneShape::JobInFront));
    assert_eq!(adopt_baseline(&wrapper, ROOT, "bash"), None);
    assert_eq!(
        adopt_baseline(&wrapper, ROOT, "claude"),
        Some(PaneBaseline::adopted(ROOT))
    );
    // Synthetic: an idle shell while tmux names another program (the two reads
    // raced): no launch baseline.
    let idle = facts(0, 0, 0, "-zsh", true).unwrap();
    assert_eq!(shell_baseline(&idle, ROOT, "node"), None);
    assert_eq!(
        shell_baseline(&idle, ROOT, "zsh"),
        Some(PaneBaseline::shell(ROOT, "zsh".into()))
    );
}

#[test]
fn proc_facts_parse_or_refuse() {
    let shells = [PathBuf::from(SHELL_EXE)];
    let exe = std::path::Path::new(SHELL_EXE);
    // A command name with spaces and parentheses does not shift the fields.
    let stat = "4242 (tmux: (odd) name) S 1 4242 4242 34816 4300 4194304 100 0 0 0";
    let parsed =
        ProcFacts::parse(stat, b"/usr/local/bin/zsh\0-i\0", "4300 4301", exe, &shells).unwrap();
    assert_eq!(
        parsed,
        ProcFacts {
            process_group: 4242,
            foreground_group: 4300,
            system_shell: true,
            children: 2,
            name: "zsh".into(),
        }
    );
    assert!(
        !ProcFacts::parse(
            stat,
            b"zsh\0",
            "",
            std::path::Path::new("/usr/bin/python3"),
            &shells
        )
        .unwrap()
        .system_shell
    );
    assert!(
        !ProcFacts::parse(stat, b"zsh\0", "", exe, &[])
            .unwrap()
            .system_shell
    );
    // Every unreadable or malformed source refuses: no facts, no baseline.
    for (case, stat, cmdline, children) in [
        ("empty stat", "", &b"zsh\0"[..], ""),
        (
            "no command name",
            "4242 S 1 4242 4242 34816 4300",
            &b"zsh\0"[..],
            "",
        ),
        ("truncated stat", "4242 (zsh) S 1 4242", &b"zsh\0"[..], ""),
        (
            "non-numeric group",
            "4242 (zsh) S 1 x 4242 34816 4300",
            &b"zsh\0"[..],
            "",
        ),
        (
            "non-numeric foreground",
            "4242 (zsh) S 1 4242 4242 34816 y",
            &b"zsh\0"[..],
            "",
        ),
        ("empty command line", stat, &b""[..], ""),
        ("a bare dash", stat, &b"-\0"[..], ""),
        ("a directory as argv0", stat, &b"/usr/bin/\0"[..], ""),
        ("command line not UTF-8", stat, &b"\xff\xfe\0"[..], ""),
        ("children not pids", stat, &b"zsh\0"[..], "4300 oops"),
    ] {
        assert_eq!(
            ProcFacts::parse(stat, cmdline, children, exe, &shells),
            None,
            "{case}"
        );
    }
}

// ---- agent.clear ----

use crate::control::reservation::PaneCapture;
use crate::registry::{AcceptedStart, PaneSession};

/// The tag `capture` puts on its pane.
const TAG: &str = "p/lead";

const CLAUDE_CONVERSATION: &str = claude!("B-baseline-conversation-120x40");
/// `/clear` pasted into that conversation's editor; its completion menu is open.
const CLAUDE_STAGED: &str = claude!("B-clear-001-120x40");
const CLAUDE_CLEARED: &str = claude!("B-clear-007-120x40");
const CLAUDE_OTHER_DRAFT: &str = include_str!(
    "../../../atm-claude-adapter/tests/fixtures/screen_styled_p1_B-auto-pasted-120x40_2.1.325.txt"
);
/// A permission dialog the adapter does not read.
const CLAUDE_MODAL: &str = claude!("C3-dialog-120x40");
const CLAUDE_TRUST: &str = claude!("A120-default-120x40");
const CODEX_CONVERSATION: &str = codex!("screen_control_p1_b12-completed-before-new-100x32");
/// `/new` pasted, with Codex's one-row completion popup.
const CODEX_POPUP: &str = codex!("screen_styled_p1_b09-valid-new-draft-100x32");
const CODEX_WORKTREE: &str = codex!("screen_control_p1_b31-nodaemon-clear-dialog-100x32");
const CODEX_FRESH: &str = codex!("screen_styled_p1_b14-fresh-baseline-32-100x32");
const CODEX_DRAFT: &str = codex!("screen_styled_p1_b23-early-paste-landed-100x24");
const CODEX_DRAFT_OVER_CONVERSATION: &str = codex!("screen_styled_p1_c03-draft-before-100x24");
const CODEX_WORKING: &str = codex!("screen_styled_p1_c07-working-held-100x24");
const CODEX_HOOK_REVIEW: &str = codex!("screen_control_p1_a02-startup-100x32");
const PI_CONVERSATION: &str = pi!("screen_control_p1_B-conversation-120x40");
const PI_FRESH: &str = pi!("screen_styled_p1_A-ready-120x40");
const PI_DRAFT: &str = pi!("screen_styled_p1_B-0-event-pasted-120x40");
const PI_RETRY: &str = pi!("screen_styled_p1_R-retry-held-120x40");
const PI_SELECTOR: &str = pi!("screen_styled_p1_C3-one-selector-120x40");

/// `/new` pasted into pi's editor; a pasted command draws no completion list.
const PI_STAGED: &str =
    include_str!("../../../atm-pi-adapter/tests/fixtures/screen_p5_120x40-staged_1.5.0.styled.txt");

/// The clear command staged in each harness's editor at 80, 100 and 120
/// columns (Codex's with its one-row completion popup).
const STAGED_COMMANDS: [(Harness, &str, [&str; 3]); 3] = [
    (
        Harness::ClaudeCode,
        "/clear",
        [
            include_str!("../../../atm-claude-adapter/tests/fixtures/screen_p5_80x40-staged_2.1.325.styled.txt"),
            include_str!("../../../atm-claude-adapter/tests/fixtures/screen_p5_100x40-staged_2.1.325.styled.txt"),
            include_str!("../../../atm-claude-adapter/tests/fixtures/screen_p5_120x40-staged_2.1.325.styled.txt"),
        ],
    ),
    (
        Harness::Codex,
        "/new",
        [
            include_str!("../../../atm-codex-adapter/tests/fixtures/screen_p5_80x40-popup_0.167.0.styled.txt"),
            include_str!("../../../atm-codex-adapter/tests/fixtures/screen_p5_100x40-popup_0.167.0.styled.txt"),
            include_str!("../../../atm-codex-adapter/tests/fixtures/screen_p5_120x40-popup_0.167.0.styled.txt"),
        ],
    ),
    (
        Harness::Pi,
        "/new",
        [
            include_str!("../../../atm-pi-adapter/tests/fixtures/screen_p5_80x40-staged_1.5.0.styled.txt"),
            include_str!("../../../atm-pi-adapter/tests/fixtures/screen_p5_100x40-staged_1.5.0.styled.txt"),
            PI_STAGED,
        ],
    ),
];

/// A real frame as a guard captures it when the pane itself allows input:
/// the agent's tag, the pinned harness in front, no client focused on it.
fn unfocused(harness: Harness, fixture: &str) -> PaneCapture {
    capture(
        fixture,
        CaptureFormat::Ansi,
        Some(ROOT),
        Some(name(harness)),
    )
    .with_focus(Some(false))
}

fn submit(harness: Harness, fixture: &str, expected: &str) -> Step {
    submit_step(
        agent_screen(harness).unwrap(),
        &unfocused(harness, fixture),
        &pinned(name(harness)),
        TAG,
        expected,
    )
}

fn enter() -> Step {
    Step::Key {
        key: PaneKey::Enter,
        expect: ControlDialog::None,
        action: KeyAction::Submit,
    }
}

const NOT_STAGED: Step = Step::Wait("the command is not staged in the editor");
const NOT_RECOGNISED: Step = Step::Wait("the screen was not recognised");

#[test]
fn input_refusal_table() {
    // One real frame per harness; only the pane's own facts change.
    for (harness, fixture) in [
        (Harness::ClaudeCode, CLAUDE_CONVERSATION),
        (Harness::Codex, CODEX_CONVERSATION),
        (Harness::Pi, PI_CONVERSATION),
    ] {
        let screen = agent_screen(harness).unwrap();
        let baseline = pinned(name(harness));
        let allowed = unfocused(harness, fixture);
        // The positive twin of every row below.
        assert_eq!(input_refusal(screen, &allowed, &baseline, TAG), None);
        assert!(safe_to_paste(screen, &allowed, &baseline, TAG));
        let with = |change: &dyn Fn(&mut PaneCapture)| {
            let mut frame = allowed.clone();
            change(&mut frame);
            frame
        };
        let foreground = "the harness is not known to be the pane's foreground program";
        let rows: [(PaneCapture, &str); 10] = [
            (
                with(&|c| c.inspection.local_agent_tag = Some("p/other".into())),
                "the pane no longer carries the agent's tag",
            ),
            (
                with(&|c| c.inspection.local_agent_tag = None),
                "the pane no longer carries the agent's tag",
            ),
            (
                with(&|c| c.inspection.dead = true),
                "the program in the pane has exited",
            ),
            (
                with(&|c| c.inspection.in_mode = true),
                "the pane is in copy mode",
            ),
            // The harness exited to its shell and the frame is stale.
            (
                with(&|c| c.inspection.foreground_command = Some("zsh".into())),
                foreground,
            ),
            (
                with(&|c| c.inspection.foreground_command = Some("node".into())),
                foreground,
            ),
            (
                with(&|c| c.inspection.foreground_command = None),
                foreground,
            ),
            (with(&|c| c.inspection.pane.pane_pid = ROOT + 1), foreground),
            (with(&|c| c.focused = Some(true)), "the pane is focused"),
            (
                with(&|c| c.focused = None),
                "whether the pane is focused could not be read",
            ),
        ];
        for (frame, reason) in rows {
            assert_eq!(
                input_refusal(screen, &frame, &baseline, TAG),
                Some(reason),
                "{harness:?}"
            );
            assert!(!safe_to_paste(screen, &frame, &baseline, TAG), "{reason}");
        }
        // A pane nothing was recorded about refuses, and so does another agent's tag.
        assert_eq!(
            input_refusal(screen, &allowed, &PaneBaseline::unproven(), TAG),
            Some(foreground)
        );
        assert!(input_refusal(screen, &allowed, &baseline, "p/other").is_some());
    }
}

#[test]
fn clear_needs_a_safe_editor_and_a_conversation_where_the_screen_is_the_proof() {
    let idle = "the editor is not empty and idle";
    let unproven = "no conversation is on screen, so a clear could not be confirmed";
    for (harness, fixture, refusal) in [
        (Harness::ClaudeCode, CLAUDE_CONVERSATION, None),
        // Claude and pi prove a clear by a new session: no conversation needed.
        (Harness::ClaudeCode, CLAUDE_CLEARED, None),
        (Harness::ClaudeCode, CLAUDE_STAGED, Some(idle)),
        (Harness::ClaudeCode, CLAUDE_OTHER_DRAFT, Some(idle)),
        (Harness::ClaudeCode, CLAUDE_MODAL, Some(idle)),
        (Harness::ClaudeCode, CLAUDE_TRUST, Some(idle)),
        (Harness::Pi, PI_CONVERSATION, None),
        (Harness::Pi, PI_FRESH, None),
        (Harness::Pi, PI_DRAFT, Some(idle)),
        (Harness::Pi, PI_RETRY, Some(idle)),
        (Harness::Pi, PI_SELECTOR, Some(idle)),
        (Harness::Codex, CODEX_CONVERSATION, None),
        // Codex proves a clear by its screen: fresh before means no proof after.
        (Harness::Codex, CODEX_FRESH, Some(unproven)),
        (Harness::Codex, CODEX_DRAFT, Some(idle)),
        (Harness::Codex, CODEX_DRAFT_OVER_CONVERSATION, Some(idle)),
        (Harness::Codex, CODEX_WORKING, Some(idle)),
        (Harness::Codex, CODEX_WORKTREE, Some(idle)),
        (Harness::Codex, CODEX_POPUP, Some(idle)),
    ] {
        let screen = agent_screen(harness).unwrap();
        let baseline = pinned(name(harness));
        let frame = unfocused(harness, fixture);
        let control = screen.control(&frame, &baseline);
        assert_eq!(
            clear_refusal(policy(harness), &control),
            refusal,
            "{harness:?} {control:?}"
        );
        // The paste guard is the same rule on the guard's own capture.
        assert_eq!(
            safe_to_paste(screen, &frame, &baseline, TAG),
            refusal.is_none(),
            "{harness:?} {control:?}"
        );
    }
    // The same pi draft frame with `/new` staged is no safe editor either:
    // a second paste would append to it.
    let screen = agent_screen(Harness::Pi).unwrap();
    assert!(!safe_to_paste(
        screen,
        &unfocused(Harness::Pi, PI_STAGED),
        &pinned("pi"),
        TAG
    ));
}

#[test]
fn submit_requires_staged_true_and_dialog_none() {
    // Real frames. Claude's completion menu with exactly `/clear` selected and
    // staged is no dialog; pi draws none for a pasted command.
    assert_eq!(
        submit(Harness::ClaudeCode, CLAUDE_STAGED, "/clear"),
        enter()
    );
    assert_eq!(submit(Harness::Pi, PI_STAGED, "/new"), enter());
    for (harness, command, widths) in STAGED_COMMANDS {
        for staged in widths {
            assert_eq!(submit(harness, staged, command), enter(), "{harness:?}");
            let wrong = format!(" {command}");
            assert_eq!(submit(harness, staged, &wrong), NOT_STAGED, "{harness:?}");
        }
    }
    // Their twins: the same frames and another expected text, leading
    // whitespace included, and the same harness with something else staged.
    for expected in [" /clear", "/clea", "/clear x", "/new", ""] {
        assert_eq!(
            submit(Harness::ClaudeCode, CLAUDE_STAGED, expected),
            NOT_STAGED,
            "{expected:?}"
        );
    }
    assert_eq!(submit(Harness::Pi, PI_STAGED, " /new"), NOT_STAGED);
    for (harness, fixture, command) in [
        (Harness::ClaudeCode, CLAUDE_CONVERSATION, "/clear"),
        (Harness::ClaudeCode, CLAUDE_CLEARED, "/clear"),
        (Harness::ClaudeCode, CLAUDE_OTHER_DRAFT, "/clear"),
        (Harness::Pi, PI_DRAFT, "/new"),
        (Harness::Pi, PI_CONVERSATION, "/new"),
        (Harness::Codex, CODEX_DRAFT, "/new"),
        (Harness::Codex, CODEX_CONVERSATION, "/new"),
    ] {
        assert_eq!(submit(harness, fixture, command), NOT_STAGED, "{harness:?}");
    }
    // Codex draws a one-row completion popup for a pasted `/new`. Its adapter
    // reads exactly that popup, with `/new` selected and staged, as no dialog,
    // so the Enter follows.
    let codex = agent_screen(Harness::Codex).unwrap();
    let popup = unfocused(Harness::Codex, CODEX_POPUP);
    assert_eq!(codex.staged(&popup, &pinned("codex"), "/new"), Some(true));
    assert_eq!(submit(Harness::Codex, CODEX_POPUP, "/new"), enter());
    assert!(submit_allowed(codex, &popup, &pinned("codex"), TAG, "/new"));
    assert_eq!(submit(Harness::Codex, CODEX_POPUP, "/ne"), NOT_STAGED);
    // A screen the adapter does not recognise is looked at again.
    assert_eq!(
        submit(Harness::ClaudeCode, CLAUDE_MODAL, "/clear"),
        NOT_RECOGNISED
    );
    // A recognised dialog stops the flow.
    for (harness, fixture, command) in [
        (Harness::ClaudeCode, CLAUDE_TRUST, "/clear"),
        (Harness::Codex, CODEX_WORKTREE, "/new"),
        (Harness::Codex, CODEX_HOOK_REVIEW, "/new"),
    ] {
        assert!(
            matches!(submit(harness, fixture, command), Step::Stuck(_)),
            "{harness:?}"
        );
    }

    // Synthetic: the rule over plain reads, for what no frame above shows.
    // Enter needs the exact draft and a positive "no dialog", both.
    assert_eq!(
        decide_submit(None, Some(true), &ControlDialog::None),
        enter()
    );
    // A whitespace-only draft with the command pasted after it is not the
    // command (whitespace_draft_plus_clear_never_gets_enter), and neither is
    // a draft the adapter can't read.
    for staged in [Some(false), None] {
        assert_eq!(
            decide_submit(None, staged, &ControlDialog::None),
            NOT_STAGED
        );
        assert_eq!(
            decide_submit(None, staged, &ControlDialog::Unknown),
            NOT_RECOGNISED
        );
    }
    // A popup with another row selected, a candidate above the selected row or
    // a selected row without its style stays Unknown in the adapter: no Enter,
    // whatever the editor row holds.
    assert_eq!(
        decide_submit(None, Some(true), &ControlDialog::Unknown),
        NOT_RECOGNISED
    );
    for dialog in [
        ControlDialog::Blocked,
        ControlDialog::LaunchSkip {
            key: ControlKey::Escape,
        },
        ControlDialog::CurrentWorktree {
            selected: true,
            accept: ControlKey::Enter,
        },
    ] {
        assert!(
            matches!(decide_submit(None, Some(true), &dialog), Step::Stuck(_)),
            "{dialog:?}"
        );
    }
    // What the pane itself refuses comes first, and waits.
    assert_eq!(
        decide_submit(
            Some("the pane is focused"),
            Some(true),
            &ControlDialog::None
        ),
        Step::Wait("the pane is focused")
    );
}

#[test]
fn submit_refuses_focused_or_unknown_focus() {
    for (harness, fixture, command) in [
        (Harness::ClaudeCode, CLAUDE_STAGED.to_owned(), "/clear"),
        (Harness::Pi, PI_STAGED.to_owned(), "/new"),
    ] {
        let screen = agent_screen(harness).unwrap();
        let baseline = pinned(name(harness));
        let staged = unfocused(harness, &fixture);
        // The twin: nobody is looking at the pane.
        assert!(submit_allowed(screen, &staged, &baseline, TAG, command));
        for (focused, reason) in [
            (Some(true), "the pane is focused"),
            (None, "whether the pane is focused could not be read"),
        ] {
            let frame = staged.clone().with_focus(focused);
            assert!(!submit_allowed(screen, &frame, &baseline, TAG, command));
            assert_eq!(
                submit_step(screen, &frame, &baseline, TAG, command),
                Step::Wait(reason)
            );
        }
        // And every other reason the pane gives, on the same staged frame.
        let mut stale = staged.clone();
        stale.inspection.foreground_command = Some("zsh".into());
        assert!(!submit_allowed(screen, &stale, &baseline, TAG, command));
        assert!(!submit_allowed(
            screen, &staged, &baseline, "p/other", command
        ));
        assert!(!submit_allowed(
            screen,
            &staged,
            &PaneBaseline::unproven(),
            TAG,
            command
        ));
    }
}

#[test]
fn a_safe_editor_is_empty_idle_and_has_no_dialog_of_either_kind() {
    // Real frames: an idle editor of each harness, with and without a
    // conversation.
    for (harness, fixture) in [
        (Harness::ClaudeCode, CLAUDE_CONVERSATION),
        (Harness::ClaudeCode, CLAUDE_CLEARED),
        (Harness::Codex, CODEX_CONVERSATION),
        (Harness::Codex, CODEX_FRESH),
        (Harness::Pi, PI_CONVERSATION),
        (Harness::Pi, PI_FRESH),
    ] {
        assert!(safe_editor(&screen(harness, fixture)), "{harness:?}");
    }
    for (harness, fixture) in [
        (Harness::ClaudeCode, CLAUDE_OTHER_DRAFT),
        (Harness::ClaudeCode, CLAUDE_MODAL),
        (Harness::ClaudeCode, CLAUDE_TRUST),
        (Harness::Codex, CODEX_WORKING),
        (Harness::Codex, CODEX_WORKTREE),
        (Harness::Pi, PI_RETRY),
        (Harness::Pi, PI_SELECTOR),
    ] {
        assert!(!safe_editor(&screen(harness, fixture)), "{harness:?}");
    }
    assert!(!safe_editor(&ControlScreen::default()));
    // Synthetic: no adapter reports an idle editor together with a control
    // dialog today, so each of the four conditions is changed alone on a real
    // idle read.
    let idle = screen(Harness::Pi, PI_FRESH);
    let changed = |change: &dyn Fn(&mut ControlScreen)| {
        let mut control = idle.clone();
        change(&mut control);
        control
    };
    for control in [
        changed(&|c| c.dialog = ControlDialog::Blocked),
        changed(&|c| c.dialog = ControlDialog::Unknown),
        changed(&|c| c.facts.input = atm_core::InputState::Draft),
        changed(&|c| c.facts.turn = atm_core::TurnState::Running),
        changed(&|c| c.facts.dialog = atm_core::DialogState::Other),
    ] {
        assert!(!safe_editor(&control), "{control:?}");
    }
}

#[test]
fn codex_noop_needs_all_three() {
    let read = |harness, fixture| screen(harness, fixture);
    let codex = policy(Harness::Codex);
    let fresh = read(Harness::Codex, CODEX_FRESH);
    // Ready without a session, no session on the pane that could be the
    // agent's, a fresh safe editor.
    assert!(clear_noop(codex, false, 0, &fresh));
    // Each one missing sends the command instead (or refuses, by `clear_refusal`).
    assert!(!clear_noop(codex, true, 0, &fresh));
    assert!(!clear_noop(codex, false, 1, &fresh));
    for fixture in [
        // A conversation whose hook was missed is not nothing to clear.
        CODEX_CONVERSATION,
        CODEX_DRAFT,
        CODEX_WORKING,
        CODEX_POPUP,
        CODEX_WORKTREE,
    ] {
        let control = read(Harness::Codex, fixture);
        assert!(!clear_noop(codex, false, 0, &control), "{control:?}");
    }
    assert!(!clear_noop(codex, false, 0, &ControlScreen::default()));
    // A harness that announces its session at the start has no such case.
    for (harness, fixture) in [
        (Harness::ClaudeCode, CLAUDE_CLEARED),
        (Harness::Pi, PI_FRESH),
    ] {
        assert!(!clear_noop(
            policy(harness),
            false,
            0,
            &read(harness, fixture)
        ));
    }
}

/// A session row on the agent's pane. `start`: `None` for a row no
/// SessionStart was accepted for, else that start's source.
fn pane_row(id: &str, harness: Harness, start: Option<Option<&str>>) -> PaneSession {
    let mut row = atm_core::SessionDomain::new(
        SessionId::new(id),
        atm_core::AgentType::GeneralPurpose,
        atm_core::Model::default(),
    );
    row.harness = harness;
    row.tmux_pane = Some("%1".into());
    PaneSession {
        view: atm_core::SessionView::from_domain(&row),
        pid: Some(4242),
        start: start.map(|source| AcceptedStart {
            source: source.map(Into::into),
        }),
    }
}

#[test]
fn clear_table_changed_session() {
    let claude = policy(Harness::ClaudeCode);
    let pi = policy(Harness::Pi);
    let before = [SessionId::new("old")];
    let new = SessionId::new("new");
    let found = |policy, harness: Harness, rows: &[PaneSession]| {
        cleared_session(policy, harness.short_tag(), rows, &before)
    };
    let old = |harness| pane_row("old", harness, Some(Some("startup")));
    let cleared = |harness| pane_row("new", harness, Some(Some("clear")));

    // Which session a clear produced.
    let c = Harness::ClaudeCode;
    assert_eq!(found(claude, c, &[]), Ok(None));
    assert_eq!(found(claude, c, &[old(c)]), Ok(None));
    assert_eq!(
        found(claude, c, &[old(c), cleared(c)]),
        Ok(Some(new.clone()))
    );
    // The old row may leave before or after the new one arrives.
    assert_eq!(found(claude, c, &[cleared(c)]), Ok(Some(new.clone())));
    // A row a status line created has had no start; another source is not
    // this clear's (wrong_Claude_source_rejected).
    assert_eq!(
        found(claude, c, &[old(c), pane_row("new", c, None)]),
        Ok(None)
    );
    for source in [Some("startup"), Some("resume"), None] {
        let row = pane_row("new", c, Some(source));
        assert_eq!(found(claude, c, &[old(c), row]), Ok(None), "{source:?}");
    }
    // The session on the pane before the command is never the result, whatever
    // it reports later (old_row_never_rebound).
    assert_eq!(
        found(claude, c, &[pane_row("old", c, Some(Some("clear")))]),
        Ok(None)
    );
    // Not the agent's: a subagent's row, a pending row, another harness's.
    let mut child = cleared(c);
    child.view.parent_session_id = Some(SessionId::new("old"));
    let pending = pane_row("pending-4242", c, Some(Some("clear")));
    assert!(pending.view.id.is_pending());
    let foreign = pane_row("new", Harness::Pi, Some(Some("clear")));
    assert_eq!(
        found(claude, c, &[old(c), child, pending, foreign]),
        Ok(None)
    );
    // Two new sessions: nothing says which is the agent's.
    let other = pane_row("newer", c, Some(Some("clear")));
    assert!(found(claude, c, &[cleared(c), other]).is_err());

    // pi names no source: any accepted start of a new id, but still a start.
    let p = Harness::Pi;
    for source in [Some("new"), Some("startup"), None] {
        let row = pane_row("new", p, Some(source));
        assert_eq!(
            found(pi, p, &[old(p), row]),
            Ok(Some(new.clone())),
            "{source:?}"
        );
    }
    assert_eq!(found(pi, p, &[old(p), pane_row("new", p, None)]), Ok(None));
    // A harness whose clear announces no session never has one to wait for.
    let x = Harness::Codex;
    assert_eq!(found(policy(x), x, &[old(x), cleared(x)]), Ok(None));

    // When the clear is done: the session and, with it, a safe editor.
    let none = ClearProgress::default();
    for (harness, fresh, conversation, staged) in [
        (
            c,
            CLAUDE_CLEARED,
            CLAUDE_CONVERSATION,
            CLAUDE_STAGED.to_owned(),
        ),
        (p, PI_FRESH, PI_CONVERSATION, PI_STAGED.to_owned()),
    ] {
        let policy = policy(harness);
        let waiting = Step::Wait("the harness has not announced a new session");
        // editor_before_event_stays_clearing: an empty editor proves nothing.
        for fixture in [fresh, conversation] {
            assert_eq!(
                decide_clear(policy, &screen(harness, fixture), &none, None),
                waiting,
                "{harness:?}"
            );
        }
        // event_before_editor_stays_clearing: the command is still in the editor.
        assert_eq!(
            decide_clear(policy, &screen(harness, &staged), &none, Some(&new)),
            Step::Wait("the editor is not empty and idle"),
            "{harness:?}"
        );
        assert_eq!(
            decide_clear(policy, &screen(harness, fresh), &none, Some(&new)),
            Step::Ready(Ready::Session(new.clone())),
            "{harness:?}"
        );
    }
    // pi_retry_empty_idle_not_ready: an empty editor under pi's retry notice.
    assert_eq!(
        decide_clear(pi, &screen(p, PI_RETRY), &none, Some(&new)),
        Step::Wait("the editor is not empty and idle")
    );
    // An unread screen is looked at again; a recognised dialog stops the clear.
    assert_eq!(
        decide_clear(claude, &screen(c, CLAUDE_MODAL), &none, Some(&new)),
        NOT_RECOGNISED
    );
    assert_eq!(
        decide_clear(claude, &ControlScreen::default(), &none, Some(&new)),
        NOT_RECOGNISED
    );
    assert_eq!(
        stuck(Some(&decide_clear(
            claude,
            &screen(c, CLAUDE_TRUST),
            &none,
            Some(&new)
        ))),
        "a dialog opened during the clear"
    );
}

#[test]
fn clear_table_fresh() {
    let x = Harness::Codex;
    let codex = policy(x);
    let none = ClearProgress::default();
    let step = |fixture, progress: &ClearProgress, session: Option<&SessionId>| {
        decide_clear(codex, &screen(x, fixture), progress, session)
    };
    // Before: the conversation. After: a fresh, safe editor without it.
    assert_eq!(
        step(CODEX_CONVERSATION, &none, None),
        Step::Wait("the conversation is still on screen")
    );
    assert_eq!(
        step(CODEX_WORKING, &none, None),
        Step::Wait("the conversation is still on screen")
    );
    assert_eq!(step(CODEX_FRESH, &none, None), Step::Ready(Ready::Editor));
    // Never a session: Codex announces the new one with its next prompt, and
    // a row that is on the pane now is the old conversation's.
    let row = SessionId::new("old-or-new");
    assert_eq!(
        step(CODEX_FRESH, &none, Some(&row)),
        Step::Ready(Ready::Editor)
    );
    // The command still staged under its popup, or a draft: look again. So is
    // a screen the adapter does not read.
    for fixture in [CODEX_POPUP, CODEX_DRAFT] {
        assert_eq!(
            step(fixture, &none, None),
            Step::Wait("no fresh editor is shown")
        );
    }
    assert_eq!(
        decide_clear(codex, &ControlScreen::default(), &none, None),
        NOT_RECOGNISED
    );

    // exact_worktree_default_only: the worktree choice is answered once, on
    // the dialog that has the current worktree selected.
    let worktree = screen(x, CODEX_WORKTREE);
    assert_eq!(
        worktree.dialog,
        ControlDialog::CurrentWorktree {
            selected: true,
            accept: ControlKey::Enter
        }
    );
    assert_eq!(
        step(CODEX_WORKTREE, &none, None),
        Step::Key {
            key: PaneKey::Enter,
            expect: worktree.dialog.clone(),
            action: KeyAction::KeepWorktree,
        }
    );
    let kept = ClearProgress {
        worktree_kept: true,
    };
    assert_eq!(
        step(CODEX_WORKTREE, &kept, None),
        Step::Wait("the worktree choice is still shown after it was answered")
    );
    assert_eq!(step(CODEX_FRESH, &kept, None), Step::Ready(Ready::Editor));
    // Synthetic: the same dialog with another option selected, and with a key
    // the control actor has no route for.
    let mut other = worktree.clone();
    other.dialog = ControlDialog::CurrentWorktree {
        selected: false,
        accept: ControlKey::Enter,
    };
    assert_eq!(
        stuck(Some(&decide_clear(codex, &other, &none, None))),
        "the harness asks which worktree to use and the current one is not selected"
    );
    other.dialog = ControlDialog::CurrentWorktree {
        selected: true,
        accept: ControlKey::Character('y'),
    };
    assert!(matches!(
        decide_clear(codex, &other, &none, None),
        Step::Stuck(_)
    ));
    // Any other recognised dialog stops the clear.
    assert_eq!(
        stuck(Some(&step(CODEX_HOOK_REVIEW, &none, None))),
        "a dialog opened during the clear"
    );
}

#[test]
fn placement_reads_a_side_and_an_agent_name() {
    use atm_tmux::PaneDirection::{Above, Below, Left, Right};
    for (hint, expected) in [
        ("below:lead", Some((Below, "lead"))),
        ("above:lead", Some((Above, "lead"))),
        ("left:a-b", Some((Left, "a-b"))),
        ("right:lead", Some((Right, "lead"))),
        // Only the first colon divides; the name is checked as an agent's later.
        ("right:a:b", Some((Right, "a:b"))),
        ("below:", None),
        ("under:lead", None),
        ("Below:lead", None),
        ("lead", None),
        ("", None),
    ] {
        assert_eq!(placement(hint), expected, "{hint:?}");
    }
}

#[test]
fn started_session_is_the_one_accepted_start_on_the_pane() {
    let found = |rows: &[PaneSession]| started_session("claude", rows);
    let started = |id| pane_row(id, Harness::ClaudeCode, Some(Some("startup")));
    assert_eq!(found(&[]), Ok(None));
    assert_eq!(found(&[started("s1")]), Ok(Some(SessionId::new("s1"))));
    // Any accepted start counts, whatever its source.
    let resumed = pane_row("s1", Harness::ClaudeCode, Some(None));
    assert_eq!(found(&[resumed]), Ok(Some(SessionId::new("s1"))));
    // A row no start created waits.
    let status_only = pane_row("s1", Harness::ClaudeCode, None);
    assert_eq!(found(&[status_only]), Ok(None));
    // Another harness's row, a child row and a pending row are not the agent's.
    let other = pane_row("p1", Harness::Pi, Some(Some("startup")));
    let mut child = started("c1");
    child.view.parent_session_id = Some(SessionId::new("s1"));
    let pending = started("pending-100");
    assert!(pending.view.id.is_pending());
    assert_eq!(
        found(&[other.clone(), child.clone(), pending.clone()]),
        Ok(None)
    );
    assert_eq!(
        found(&[other, child, pending, started("s1")]),
        Ok(Some(SessionId::new("s1")))
    );
    // Two of the agent's harness: nothing says which is the agent's.
    assert!(found(&[started("s1"), started("s2")]).is_err());
    assert!(found(&[started("s1"), pane_row("s2", Harness::ClaudeCode, None)]).is_err());
}

#[test]
fn launch_guards_need_the_recorded_shell_unfocused_and_the_exact_line() {
    let screen = agent_screen(Harness::Codex).unwrap();
    let baseline = PaneBaseline::shell(ROOT, "zsh".into());
    let text = "cd '/proj' && ATM_AGENT='p/lead' codex";
    let shell = |line: &str| {
        shell_capture(&[line, ""], 80, (line.len() as u32, 0), CaptureFormat::Ansi)
            .with_focus(Some(false))
    };
    let staged = format!("pb> {text}");
    let allowed = |before: &PaneCapture, after: &PaneCapture| {
        (
            shell_refusal(screen, before, &baseline, "p/lead"),
            launch_paste_allowed(screen, before, &baseline, "p/lead", "pb> "),
            launch_enter_allowed(screen, after, &baseline, "p/lead", ("pb> ", text)),
        )
    };
    assert_eq!(allowed(&shell("pb> "), &shell(&staged)), (None, true, true));
    // Each change to the pane refuses both guards, with its reason.
    type Change = fn(&mut PaneCapture);
    let changes: [(Change, &str); 8] = [
        (
            |c| c.inspection.local_agent_tag = Some("p/other".into()),
            "the pane no longer carries the agent's tag",
        ),
        (
            |c| c.inspection.dead = true,
            "the shell in the pane has exited",
        ),
        (|c| c.inspection.in_mode = true, "the pane is in copy mode"),
        (
            |c| c.inspection.pane.pane_pid += 1,
            "the pane's shell is not known to be its foreground program",
        ),
        (
            |c| c.inspection.foreground_command = Some("vim".into()),
            "the pane's shell is not known to be its foreground program",
        ),
        (
            |c| c.inspection.foreground_command = None,
            "the pane's shell is not known to be its foreground program",
        ),
        (|c| c.focused = Some(true), "the pane is focused"),
        (
            |c| c.focused = None,
            "whether the pane is focused could not be read",
        ),
    ];
    for (change, reason) in changes {
        let (mut before, mut after) = (shell("pb> "), shell(&staged));
        change(&mut before);
        change(&mut after);
        assert_eq!(
            allowed(&before, &after),
            (Some(reason), false, false),
            "{reason}"
        );
    }
    // The line: the paste needs it to read the prompt, the Enter the prompt
    // followed by the command.
    assert_eq!(
        allowed(&shell("pb> x"), &shell(&format!("pb> x{text}"))),
        (None, false, false)
    );
    assert_eq!(
        allowed(&shell(&staged), &shell("pb> ")),
        (None, false, false)
    );
    // A harness's screen in a pane whose shell is in front (a stale frame):
    // nothing is typed under it. Real capture: Codex's fresh editor.
    let fresh = include_str!(
        "../../../atm-codex-adapter/tests/fixtures/screen_styled_p1_b14-fresh-baseline-32-100x32_0.167.0.txt"
    );
    let mut stale =
        capture(fresh, CaptureFormat::Ansi, Some(ROOT), Some("zsh")).with_focus(Some(false));
    stale.inspection.local_agent_tag = Some("p/lead".into());
    assert_eq!(
        shell_refusal(screen, &stale, &baseline, "p/lead"),
        Some("the pane shows a harness's screen before anything was launched")
    );
    assert!(screen.recognises(&stale));
    assert!(!screen.recognises(&shell("pb> ")));
}
