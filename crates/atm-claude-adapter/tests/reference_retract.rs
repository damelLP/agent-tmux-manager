//! atm's own reference staged in Claude's editor, then one Ctrl-U (unit s3-5c).
//! Each fixture's first line is its `Captured:` provenance: a sealed rig
//! (private user and network namespace, canned Messages API), the reference
//! pasted as atm pastes it (bracketed), tmux 3.8 `capture-pane -p -e`; 2.1.330
//! with terminal focus on the pane, 2.1.327 without. After the key the editor
//! is empty again (the prompt suggestion is back) and nothing was submitted.
//! Both versions then show a right-aligned `ctrl+y to paste deleted text` row
//! above the editor for 5.1 s (measured twice each on a fresh seat, 2026-11-21),
//! and the `settled` fixtures are the first frames without it.
//!
//! That row is not read as chrome: an earlier unit pins the 2.1.326 frame that
//! shows it after `/clear` as proving neither idle nor freshness. So for those
//! 5 s the turn reads Unknown, and a delivery waits; the editor reads Empty at
//! once, which is what atm checks after its key.
use atm_claude_adapter::screen::{observe_control_screen, observe_screen, staged_draft_matches};
use atm_core::harness_control::ControlDialog;
use atm_core::{DialogState, InputState, ScreenInput, TurnState};

const CAPTURES: [(&str, &str, &str); 2] = [
    (
        "[m99330] Read: atm inbox --id m99330",
        include_str!("fixtures/claude_reference_staged_82x60_2.1.330.styled.txt"),
        include_str!("fixtures/claude_reference_after_ctrl_u_82x60_2.1.330.styled.txt"),
    ),
    (
        "[m99327] Read: atm inbox --id m99327",
        include_str!("fixtures/claude_reference_staged_82x60_2.1.327.styled.txt"),
        include_str!("fixtures/claude_reference_after_ctrl_u_82x60_2.1.327.styled.txt"),
    ),
];
const SETTLED: [&str; 2] = [
    include_str!("fixtures/claude_reference_after_ctrl_u_settled_82x60_2.1.330.styled.txt"),
    include_str!("fixtures/claude_reference_after_ctrl_u_settled_82x60_2.1.327.styled.txt"),
];

fn body(capture: &str) -> &str {
    match capture.split_once('\n') {
        Some((first, rest)) if first.starts_with("Captured:") => rest,
        _ => panic!("expected a provenance line above the capture"),
    }
}

// Break: atm's staged reference is not an exact single-row match in an idle
// editor, so the retract guard never allows the key and the reference stays.
#[test]
fn staged_reference_is_an_exact_match_in_an_idle_editor() {
    for (reference, staged, _) in CAPTURES {
        let input = ScreenInput::Styled(body(staged));
        let control = observe_control_screen(input, None);
        assert_eq!(control.dialog, ControlDialog::None, "{reference}");
        assert_eq!(control.facts.input, InputState::Draft, "{reference}");
        assert_eq!(control.facts.turn, TurnState::NotRunning, "{reference}");
        assert_eq!(control.facts.dialog, DialogState::None, "{reference}");
        assert_eq!(observe_screen(input, None), control.facts);
        assert_eq!(staged_draft_matches(input, reference), Some(true));
        // Another message's reference is not this one.
        assert_eq!(
            staged_draft_matches(input, "[m1] Read: atm inbox --id m1"),
            Some(false)
        );
    }
}

// Break: the screen right after Ctrl-U does not read as an empty editor, so
// atm reports its own retract as failed; or the `ctrl+y` row it shows starts
// to prove an idle turn (it must stay as unproven as after `/clear`).
#[test]
fn after_ctrl_u_the_editor_reads_empty_and_the_turn_waits_for_the_row() {
    for (reference, _, after) in CAPTURES {
        let text = body(after);
        assert!(text.contains("ctrl+y to paste deleted text"), "{reference}");
        let input = ScreenInput::Styled(text);
        let control = observe_control_screen(input, None);
        assert_eq!(control.dialog, ControlDialog::None, "{reference}");
        assert_eq!(control.facts.input, InputState::Empty, "{reference}");
        assert_eq!(control.facts.turn, TurnState::Unknown, "{reference}");
        assert_eq!(control.facts.dialog, DialogState::None, "{reference}");
        assert_eq!(observe_screen(input, None), control.facts);
        assert_ne!(staged_draft_matches(input, reference), Some(true));
    }
}

// Break: once the row is gone the seat is still not an idle empty editor, so
// the delivery behind atm's retracted reference never goes out.
#[test]
fn once_the_row_is_gone_the_editor_is_empty_and_idle() {
    for settled in SETTLED {
        let text = body(settled);
        assert!(!text.contains("paste deleted text"));
        let input = ScreenInput::Styled(text);
        let control = observe_control_screen(input, None);
        assert_eq!(control.dialog, ControlDialog::None);
        assert_eq!(control.facts.input, InputState::Empty);
        assert_eq!(control.facts.turn, TurnState::NotRunning);
        assert_eq!(control.facts.dialog, DialogState::None);
        assert_eq!(observe_screen(input, None), control.facts);
    }
}
