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
fn launch_line_refuses_what_it_cannot_measure() {
    // Synthetic. A character before the cursor that is not printable ASCII may
    // be wider than one cell: the cut at the cursor is then unknown.
    for prompt in ["\u{276f} ", "\u{1f680} ", "caf\u{e9}> "] {
        let capture = shell_capture(
            &[prompt, ""],
            80,
            (prompt.chars().count() as u32, 0),
            CaptureFormat::Plain,
        );
        assert_eq!(launch_line(&capture), None, "{prompt:?}");
    }
    // A row above that may or may not fill the width: unknown, so refuse.
    let wide = "\u{1f680}".repeat(40);
    let capture = shell_capture(&[&wide, "pb> "], 80, (4, 1), CaptureFormat::Plain);
    assert_eq!(launch_line(&capture), None);
    // The twin: a row above too short to fill the width under any widths.
    let capture = shell_capture(&["\u{276f} done", "pb> "], 80, (4, 1), CaptureFormat::Plain);
    assert_eq!(launch_line(&capture).as_deref(), Some("pb> "));
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
