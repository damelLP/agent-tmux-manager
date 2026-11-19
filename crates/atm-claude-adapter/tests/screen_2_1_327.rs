//! Captured: claude 2.1.327, 2026-11-18, live capture-pane -p -e -N (lead), sanitized.
//! Bottom rows retain the completed or active spinner and the editor chrome.
//! Suggestion, Tip, and custom status text are fictionalized in place.

use atm_claude_adapter::screen;
use atm_core::harness_control::{ControlDialog, ControlScreen};
use atm_core::{DialogState, InputState, ScreenFacts, ScreenInput, TurnState};

const IDLE: &str = include_str!("fixtures/claude_shells_idle_2.1.327.styled.txt");
const BUSY_TIP: &str = include_str!("fixtures/claude_shells_busy_tip_2.1.327.styled.txt");
const BUSY_DIM: &str = include_str!("fixtures/claude_shells_busy_dim_2.1.327.styled.txt");

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
fn captured_idle_managed_shells() {
    assert_shell_editor(IDLE, TurnState::NotRunning);
}

#[test]
fn captured_running_managed_shell_with_tip() {
    assert_shell_editor(BUSY_TIP, TurnState::Running);
}

#[test]
fn captured_running_managed_shell_with_dim_spinner() {
    assert_shell_editor(BUSY_DIM, TurnState::Running);
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
fn management_hint_requires_exact_shell_footer() {
    // Synthetic: replace only the last row; misspelling, displacement, unknown
    // segments and malformed counts must not make the completed frame ready.
    for footer in [
        "  ⏵⏵ auto mode on · 8 shells · ↓ to manag",
        " ⏵⏵ auto mode on · 8 shells · ↓ to manage",
        "   ⏵⏵ auto mode on · 8 shells · ↓ to manage",
        "  ⏵⏵ auto mode on · 8 shells  · ↓ to manage",
        "  ⏵⏵ auto mode on · 8 shells  · ↓ to manag",
        "  ⏵⏵ auto mode on · 8 shells  ↓ to manage",
        "  ⏵⏵ auto mode on · 8 shells ·  ↓ to manage",
        "  ⏵⏵ auto mode on · 8 shells · ↓ to manage  novel",
        "  ⏵⏵ auto mode on · ↓ to manage",
        "  ⏵⏵ auto mode on · 1 shells · ↓ to manage",
        "  ⏵⏵ auto mode on · 08 shells · ↓ to manage",
        "  ⏵⏵ auto mode on · 8 shells · ↓ to manage · ← for agents",
        "  ⏵⏵ auto mode on · 8 shells · ↓ to manage · esc to interrupt",
        "  ⏸ manual mode on · 8 shells · ↓ to manage",
    ] {
        assert_refused_with_footer(IDLE, footer);
    }
}

#[test]
fn running_management_footer_requires_both_segments_in_order() {
    // Synthetic: preserve the real running status; footer damage alone refuses.
    for footer in [
        "  ⏵⏵ auto mode on · 1 shell · esc to interrupt",
        "  ⏵⏵ auto mode on · 1 shell · esc to interupt · ↓ to manage",
        "  ⏵⏵ auto mode on · 1 shell  esc to interrupt · ↓ to manage",
        "  ⏵⏵ auto mode on · 1 shell · ↓ to manage · esc to interrupt",
    ] {
        assert_refused_with_footer(BUSY_TIP, footer);
    }
}

#[test]
fn busy_management_footer_refuses_completed_history() {
    // Synthetic: an exact busy footer contradicts the real completed spinner.
    assert_refused_with_footer(
        IDLE,
        "  ⏵⏵ auto mode on · 1 shell · esc to interrupt · ↓ to manage",
    );
}
