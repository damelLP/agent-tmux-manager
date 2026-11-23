//! Captured: Claude Code 2.1.327, 2026-11-22, tmux capture-pane -p -e -N.

use atm_claude_adapter::screen::{
    inbox_survey_ready, legacy_send_veto, observe_control_screen, observe_screen,
};
use atm_core::{DialogState, InputState, ScreenInput, TurnState};

const SURVEY: &str = include_str!("fixtures/claude_feedback_survey_2.1.327.styled.txt");
const HEADING: &str = "How is Claude doing this session? (optional)";
const DONE: &str = "Crunched for 27s · done 12:15";

fn viewport() -> String {
    SURVEY
        .lines()
        .skip(1)
        .map(|row| match row.strip_suffix('|') {
            Some(text) => text,
            None => panic!("expected a fixture row terminator"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn captured_survey_offers_only_opt_in_inbox_paste_readiness() {
    let frame = viewport();
    let styled = ScreenInput::Styled(&frame);
    let ordinary = observe_screen(styled, None);
    assert_eq!(
        (ordinary.input, ordinary.turn, &ordinary.dialog),
        (InputState::Empty, TurnState::Unknown, &DialogState::None)
    );
    assert_eq!(observe_control_screen(styled, None).facts, ordinary);
    assert!(inbox_survey_ready(styled));

    // Synthetic: no update notice, and the original banner has scrolled away.
    let no_banner_or_notice = frame
        .lines()
        .skip(4)
        .filter(|row| !row.contains("✔ Update installed"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(inbox_survey_ready(ScreenInput::Styled(
        &no_banner_or_notice
    )));
}

#[test]
fn the_survey_veto_does_not_require_inbox_paste_readiness() {
    let frame = viewport();
    // Synthetic: these same anchored survey rows stay visible; only the named
    // condition changes, and none gives the inbox scheduler paste permission.
    for altered in [
        frame.replace(DONE, "unproven history"),
        frame.replace(
            "(shift+tab to cycle)",
            "(shift+tab to cycle) · esc to interrupt",
        ),
        frame.replacen("❯\u{a0} ", "❯\u{a0}１", 1),
    ] {
        assert_ne!(altered, frame);
        let input = ScreenInput::Styled(&altered);
        assert!(!inbox_survey_ready(input));
        assert!(legacy_send_veto(input, "1", Some(true)).is_some());
    }
    assert!(legacy_send_veto(ScreenInput::Plain(&frame), "1", Some(true)).is_none());
}

#[test]
fn survey_refuses_single_character_choices_and_leaves_fours_alone() {
    let frame = viewport();
    let input = ScreenInput::Styled(&frame);
    // A missed normalized choice or layout alias would be pasted into the survey.
    for (choices, refused) in [
        ("0123", true),
        ("０１２３", true),
        ("₀₁₂₃", true),
        ("⓪①②③", true),
        ("⁰¹²³", true),
        ("&é\"à", true),
        ("'(４4", false),
    ] {
        for choice in choices.chars() {
            assert_eq!(
                legacy_send_veto(input, &choice.to_string(), Some(true)).is_some(),
                refused,
                "{choice:?}"
            );
        }
    }
}

#[test]
fn a_displaced_header_does_not_overwrite_ordinary_facts_with_a_busy_contradiction() {
    let frame = viewport();
    // Synthetic: preserve the captured header and an empty editor, but the
    // ordinary and control observers disagree about where the header starts.
    let header = frame.lines().skip(1).take(3).collect::<Vec<_>>().join("\n");
    let editor = frame
        .lines()
        .skip(115)
        .collect::<Vec<_>>()
        .join("\n")
        .replace(
            "(shift+tab to cycle)",
            "(shift+tab to cycle) · esc to interrupt",
        );
    let frame = format!("a prior row before the banner\n{header}\n{editor}");
    let facts = observe_control_screen(ScreenInput::Styled(&frame), None).facts;
    assert_eq!(
        (facts.input, facts.turn, facts.dialog),
        (InputState::Empty, TurnState::Unknown, DialogState::None)
    );
}

#[test]
fn inbox_survey_requires_the_exact_adjacent_pair_and_completed_turn() {
    let frame = viewport();
    let old_history = frame
        .lines()
        .flat_map(|row| {
            row.contains("✔ Update installed")
                .then_some("unrecognized history row")
                .into_iter()
                .chain([row])
        })
        .collect::<Vec<_>>()
        .join("\n");
    // Synthetic: change only the named row or slot. No unfamiliar layout,
    // missing turn evidence, or user's draft grants the opt-in readiness.
    for (label, altered) in [
        (
            "different question",
            frame.replace(HEADING, "Some other question"),
        ),
        ("partial choices", frame.replace(": Dismiss", ": Dismis")),
        (
            "misaligned heading",
            frame.replace("\n\x1b[38;5;153m●", "\n \x1b[38;5;153m●"),
        ),
        (
            "misaligned choices",
            frame.replace("\n  \x1b[38;5;153m1", "\n \x1b[38;5;153m1"),
        ),
        ("old history", old_history),
        ("no completed turn", frame.replace(DONE, "unrecognized row")),
        ("running turn", frame.replace(DONE, "Cooking…")),
        (
            "busy footer",
            frame.replace(
                "(shift+tab to cycle)",
                "(shift+tab to cycle) · esc to interrupt",
            ),
        ),
        ("draft", frame.replace("❯\u{a0}", "❯\u{a0}0")),
    ] {
        assert_ne!(altered, frame, "bad mutation: {label}");
        assert!(
            !inbox_survey_ready(ScreenInput::Styled(&altered)),
            "{label}"
        );
    }
    assert!(!inbox_survey_ready(ScreenInput::Plain(&frame)));
}
