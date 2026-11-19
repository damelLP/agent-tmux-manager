//! Captured: Claude Code 2.1.328, 2026-11-19, sealed local canned Messages API, tmux 3.8.
//! Cropped to the spinner and editor chrome; the transcript and path are omitted.

use atm_claude_adapter::screen;
use atm_core::harness_control::{ControlDialog, ControlScreen};
use atm_core::{DialogState, InputState, ScreenFacts, ScreenInput, TurnState};

const IDLE: &str = include_str!("fixtures/claude_shells_idle_2.1.328.styled.txt");
const BUSY: &str = include_str!("fixtures/claude_shells_busy_2.1.328.styled.txt");

fn body(capture: &str) -> &str {
    let Some((_, text)) = capture.split_once('\n') else {
        panic!("expected a provenance line above the capture");
    };
    text
}

fn assert_shell_editor(capture: &str, turn: TurnState) {
    let input = ScreenInput::Styled(body(capture));
    let mut expected = ScreenFacts::default();
    expected.input = InputState::Empty;
    expected.turn = turn;
    expected.dialog = DialogState::None;
    assert_eq!(screen::observe_screen(input, None), expected);
    let control = screen::observe_control_screen(input, None);
    assert_eq!(control.facts, expected);
    assert_eq!(control.dialog, ControlDialog::None);
    assert_eq!(screen::staged_draft_matches(input, "/clear"), None);
}

#[test]
fn captured_idle_managed_shell_with_agents_hint() {
    assert_shell_editor(IDLE, TurnState::NotRunning);
}

#[test]
fn captured_running_managed_shell_with_agents_hint() {
    assert_shell_editor(BUSY, TurnState::Running);
}

fn assert_refused_with_footer(capture: &str, footer: &str) {
    let Some((before_footer, _)) = body(capture).trim_end_matches('\n').rsplit_once('\n') else {
        panic!("expected both editor and footer rows");
    };
    let altered = format!("{before_footer}\n{footer}\n");
    let input = ScreenInput::Styled(&altered);
    assert_eq!(
        screen::observe_screen(input, None),
        ScreenFacts::default(),
        "{footer}"
    );
    assert_eq!(
        screen::observe_control_screen(input, None),
        ControlScreen::default(),
        "{footer}"
    );
    assert_eq!(
        screen::staged_draft_matches(input, "/clear"),
        None,
        "{footer}"
    );
}

#[test]
fn managed_shell_agents_hint_only_precedes_the_manage_hint() {
    // Synthetic: reorder the agents hint while keeping the captured editor.
    for (capture, footer) in [
        (
            IDLE,
            "  ⏵⏵ auto mode on · ← for agents · 1 shell · ↓ to manage",
        ),
        (
            IDLE,
            "  ⏵⏵ auto mode on · 1 shell · ↓ to manage · ← for agents",
        ),
        (
            BUSY,
            "  ⏵⏵ auto mode on · 1 shell · ← for agents · esc to interrupt · ↓ to manage",
        ),
    ] {
        assert_refused_with_footer(capture, footer);
    }
}

#[test]
fn managed_shell_agents_hint_does_not_hide_an_extra_segment() {
    // Synthetic: an unknown segment is not chrome, even across a column gap.
    for footer in [
        "  ⏵⏵ auto mode on · 1 shell · ← for agents · novel · ↓ to manage",
        "  ⏵⏵ auto mode on · 1 shell · ← for agents · ← for agents · ↓ to manage",
        "  ⏵⏵ auto mode on · 1 shell  novel · ← for agents · ↓ to manage",
    ] {
        assert_refused_with_footer(IDLE, footer);
    }
}

#[test]
fn managed_shell_agents_hint_cannot_be_a_right_aligned_notice() {
    // Synthetic: the legacy two-column split must not discard a displaced pill.
    assert_refused_with_footer(
        IDLE,
        "  ⏵⏵ auto mode on · 1 shell  ← for agents · ↓ to manage",
    );
}

#[test]
fn busy_managed_shell_with_agents_hint_refuses_completed_history() {
    // Synthetic: a recognized busy footer cannot make a completed turn ready.
    assert_refused_with_footer(
        IDLE,
        "  ⏵⏵ auto mode on · 1 shell · esc to interrupt · ← for agents · ↓ to manage",
    );
}
