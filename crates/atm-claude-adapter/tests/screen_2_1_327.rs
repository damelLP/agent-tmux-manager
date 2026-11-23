//! Captured: claude 2.1.327, 2026-11-18, live capture-pane -p -e -N (lead), sanitized.
//! Bottom rows retain the completed or active spinner and the editor chrome.
//! Suggestion, Tip, and custom status text are fictionalized in place.

use atm_claude_adapter::screen;
use atm_core::harness_control::{ControlDialog, ControlScreen};
use atm_core::{DialogState, InputState, ScreenFacts, ScreenInput, TurnState};

const IDLE: &str = include_str!("fixtures/claude_shells_idle_2.1.327.styled.txt");
const BUSY_TIP: &str = include_str!("fixtures/claude_shells_busy_tip_2.1.327.styled.txt");
const BUSY_DIM: &str = include_str!("fixtures/claude_shells_busy_dim_2.1.327.styled.txt");
const MONITOR_IDLE: &str = include_str!("fixtures/claude_monitor_idle_2.1.327.styled.txt");

fn body(capture: &str) -> &str {
    let Some((_, text)) = capture.split_once('\n') else {
        panic!("expected a provenance line above the capture");
    };
    text
}

fn assert_shell_editor(capture: &str, turn: TurnState) {
    assert_background_editor(capture, turn, ControlDialog::None);
}

fn assert_background_editor(capture: &str, turn: TurnState, dialog: ControlDialog) {
    let input = ScreenInput::Styled(body(capture));
    let mut expected = ScreenFacts::default();
    expected.input = InputState::Empty;
    expected.turn = turn;
    expected.dialog = DialogState::None;
    assert_eq!(screen::observe_screen(input, None), expected);
    let control = screen::observe_control_screen(input, None);
    assert_eq!(control.facts, expected);
    assert_eq!(control.dialog, dialog);
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

fn monitor_capture() -> String {
    // `|` protects the original trailing grid spaces in the recorded rows.
    let mut rows = MONITOR_IDLE.lines();
    let Some(provenance) = rows.next() else {
        panic!("expected monitor capture provenance");
    };
    let mut capture = vec![provenance];
    for row in rows {
        let Some(row) = row.strip_suffix('|') else {
            panic!("expected monitor capture row terminator");
        };
        capture.push(row);
    }
    capture.join("\n")
}

#[test]
fn captured_idle_managed_monitor() {
    // Break: a monitor footer masks the real completion above the update notice.
    assert_background_editor(
        &monitor_capture(),
        TurnState::NotRunning,
        ControlDialog::None,
    );
}

#[test]
fn managed_monitor_accepts_canonical_plural_counts() {
    // Synthetic: replace only the footer so the same completion applies.
    let capture = monitor_capture();
    let Some((before_footer, _)) = capture.rsplit_once('\n') else {
        panic!("expected a footer in the monitor capture");
    };
    for count in ["2 monitors", "10 monitors"] {
        let with_plural = format!("{before_footer}\n  ⏵⏵ auto mode on · {count} · ↓ to manage");
        assert_background_editor(&with_plural, TurnState::NotRunning, ControlDialog::None);
    }
}

#[test]
fn managed_monitor_rejects_altered_footers() {
    // Synthetic: a truncated or otherwise unverified monitor pill stays unknown
    // even with the same completed transcript and the complete update notice.
    let capture = monitor_capture();
    for footer in [
        " ⏵⏵ auto mode on · 1 monitor · ↓ to manage",
        "   ⏵⏵ auto mode on · 1 monitor · ↓ to manage",
        "  ⏵⏵ auto mode on · 0 monitors · ↓ to manage",
        "  ⏵⏵ auto mode on · 02 monitors · ↓ to manage",
        "  ⏵⏵ auto mode on · 1 monitors · ↓ to manage",
        "  ⏵⏵ auto mode on · 2 monitor · ↓ to manage",
        "  ⏵⏵ auto mode on · ２ monitors · ↓ to manage",
        "  ⏵⏵ auto mode on · 1 monitor",
        "  ⏵⏵ auto mode on · 1 monitor  · ↓ to manage",
        "  ⏵⏵ auto mode on · 1 monitor · ↓ to manag",
        "  ⏵⏵ auto mode on · 1 monitor · ↓ to manage  unknown",
        "  ⏵⏵ auto mode on · 1 monitor · esc to interrupt · ↓ to manage",
        "  ⏵⏵ auto mode on · 1 monitor · ← for agents · ↓ to manage",
        "  ⏸ manual mode on · 1 monitor · ↓ to manage",
    ] {
        assert_refused_with_footer(&capture, footer);
    }
}

#[test]
fn managed_monitor_needs_independent_turn_evidence() {
    // Synthetic: keep the recognized footer but replace the completion, first
    // with no status evidence, then with a running spinner.
    let capture = monitor_capture();
    let Some((provenance, rest)) = capture.split_once('\n') else {
        panic!("expected monitor capture provenance");
    };
    let Some((_, chrome)) = rest.split_once('\n') else {
        panic!("expected a completion row above the rest of the monitor capture");
    };
    for (status, turn, dialog) in [
        ("", TurnState::Unknown, ControlDialog::Unknown),
        ("✻ Cooking…", TurnState::Running, ControlDialog::None),
    ] {
        assert_background_editor(&format!("{provenance}\n{status}\n{chrome}"), turn, dialog);
    }
}
