//! Queued-draft refusal probes based on real Running and update-notice captures.
#![cfg(test)]
use atm_core::{
    harness_control::ControlDialog, styled_screen::StyledScreen, InputState, ScreenFacts,
    ScreenInput, TurnState,
};
use atm_pi_adapter::screen::{observe_control_screen, staged_draft_matches};

// Captured: pi 1.5.0, 2026-11-14, P1 private tmux capture-pane -p -e -N; sanitized.
const RUNNING: &str = include_str!("fixtures/screen_styled_p1_C2-steer-one-held-120x40_1.5.0.txt");
// Captured: pi 1.5.0, 2026-11-15, sealed private tmux capture-pane -p -e -N; sanitized.
const POST_TURN: &str = include_str!("fixtures/clear_post_turn_pi_1.5.0.txt");

fn running() -> &'static str {
    RUNNING.split_once('\n').unwrap().1
}

// Synthetic: retain real history, border styles and cursor; replace the editor
// with an unfinished draft, nonfaint pane-wide rule and cursor/staged-message row.
// POST_TURN also gets a Working upper border, preserving its captured color.
// This probes parser refusal, not whether live pi can render a pane-wide draft.
fn queued_rule(text: &str, staged: &str) -> String {
    let decoded = StyledScreen::parse(text).unwrap();
    let plain: Vec<_> = decoded
        .text()
        .lines()
        .map(|row| row.trim_end_matches(' '))
        .collect();
    let lower = plain.iter().rposition(|row| row.starts_with("──")).unwrap();
    let upper = plain[..lower]
        .iter()
        .rposition(|row| row.starts_with("──"))
        .unwrap();
    let width = plain[lower].chars().count();
    let rule = "─".repeat(width);
    let working = format!("── ⠋ Working {}", "─".repeat(width - 13));
    let mut rows: Vec<_> = text.lines().map(str::to_owned).collect();
    rows[upper] = rows[upper].replace(plain[upper], &working);
    let cursor = format!("\x1b[0m{staged}{}", rows[upper + 1]);
    rows.splice(
        upper + 1..lower,
        [
            "\x1b[0mkeep this unfinished".to_owned(),
            format!("\x1b[0m{rule}"),
            cursor,
        ],
    );
    rows.join("\n")
}

fn refuses_empty_editor(text: &str) {
    let result = observe_control_screen(ScreenInput::Styled(text), None);
    assert_ne!(result.facts.input, InputState::Empty);
    assert_ne!(result.facts.turn, TurnState::NotRunning);
    assert_eq!(result.facts, ScreenFacts::default());
    assert_eq!(result.dialog, ControlDialog::Unknown);
}

#[test]
fn running_queued_rule_cannot_look_empty_or_idle() {
    refuses_empty_editor(&queued_rule(running(), ""));
}

#[test]
fn running_queued_rule_cannot_match_only_the_appended_message() {
    let text = queued_rule(running(), "MSG");
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(&text), "MSG"),
        None
    );
    refuses_empty_editor(&text);
}

#[test]
fn notice_running_queued_rule_cannot_look_empty_or_idle() {
    refuses_empty_editor(&queued_rule(POST_TURN, ""));
}

#[test]
fn notice_running_queued_rule_cannot_match_only_the_appended_message() {
    let text = queued_rule(POST_TURN, "MSG");
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(&text), "MSG"),
        None
    );
    refuses_empty_editor(&text);
}

#[test]
fn unstyled_queued_rule_keeps_legacy_observation() {
    // Synthetic: plain capture drops styles and trailing padding. It cannot
    // distinguish a pasted rule; production requests Styled, never this proof.
    let text = queued_rule(running(), "");
    let decoded = StyledScreen::parse(&text).unwrap();
    let plain = decoded
        .text()
        .lines()
        .map(|row| row.trim_end_matches(' '))
        .collect::<Vec<_>>()
        .join("\n");
    let result = observe_control_screen(ScreenInput::Plain(&plain), None);
    assert_eq!(result.facts.input, InputState::Empty);
    assert_eq!(result.facts.turn, TurnState::NotRunning);
    assert_eq!(result.dialog, ControlDialog::None);
    assert_eq!(
        staged_draft_matches(ScreenInput::Plain(&plain), "MSG"),
        None
    );
}
