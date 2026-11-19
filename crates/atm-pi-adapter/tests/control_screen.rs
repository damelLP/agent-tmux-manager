use atm_core::harness_control::{ClearProof, ControlDialog, SessionArrival};
use atm_core::{
    styled_screen::StyledScreen, DialogState, FreshEditorEvidence, InputState, ScreenInput,
    TurnState,
};
use atm_pi_adapter::screen::{self, observe_control_screen, CONTROL_POLICY};
macro_rules! capture {
    ($name:literal) => {
        include_str!(concat!("fixtures/screen_styled_p1_", $name, "_1.5.0.txt"))
            .split_once('\n')
            .unwrap()
            .1
    };
}
fn observe(s: &str) -> atm_core::harness_control::ControlScreen {
    observe_control_screen(ScreenInput::Styled(s), None)
}
fn plain(s: &str) -> String {
    StyledScreen::parse(s)
        .unwrap_or_else(|error| panic!("invalid fixture styles: {error}"))
        .text()
        .to_owned()
}

#[test]
fn policy_reuses_existing_constants() {
    assert_eq!(CONTROL_POLICY.capture, screen::SCREEN_TEXT_FORMAT);
    assert_eq!(CONTROL_POLICY.clear_command, screen::CLEAR_COMMAND);
    assert_eq!(CONTROL_POLICY.session_arrival, SessionArrival::AtStart);
    assert_eq!(
        CONTROL_POLICY.clear_proof,
        ClearProof::ChangedSession { start_source: None }
    );
    assert_eq!(CONTROL_POLICY.trust_selection_retries, 0);
}

#[test]
fn fresh_and_conversation_have_independent_editor_facts() {
    for text in [capture!("A-ready-120x40"), capture!("B-fresh-120x40")] {
        let result = observe(text);
        assert_eq!(
            result.facts,
            screen::observe_screen(ScreenInput::Styled(text), None)
        );
        assert_eq!(result.dialog, ControlDialog::None);
        assert_eq!(result.facts.fresh_editor, FreshEditorEvidence::Present);
        assert!(!result.conversation);
    }
    let conversation = include_str!("fixtures/screen_control_p1_B-conversation-120x40_1.5.0.txt")
        .split_once('\n')
        .unwrap()
        .1;
    for text in [
        conversation,
        capture!("C2-steer-one-held-120x40"),
        capture!("C2-steer-one-queued-120x40"),
        capture!("C4-one-draft-120x40"),
    ] {
        assert!(observe(text).conversation, "{}", plain(text));
        assert_eq!(
            observe(text).facts,
            screen::observe_screen(ScreenInput::Styled(text), None)
        );
        assert_eq!(observe(text).dialog, ControlDialog::None);
        assert_eq!(
            observe(text).facts.fresh_editor,
            FreshEditorEvidence::Unknown
        );
    }
    // Synthetic: draft alone is not history; retained new-session notice is not freshness.
    let fresh = capture!("B-fresh-120x40");
    let draft = fresh.replace("\x1b[0;7m \x1b[0m", "typed draft");
    assert_ne!(draft, fresh);
    assert!(!observe(&draft).conversation);
    let old = plain(fresh).replace(
        "✓ New session started",
        "✓ New session started\n\n Old prompt\n\n Old reply",
    );
    assert!(observe(&old).conversation);
    assert_eq!(
        observe(&old).facts.fresh_editor,
        FreshEditorEvidence::Unknown
    );
}

#[test]
fn captured_markdown_link_does_not_hide_the_idle_editor() {
    let fixture = include_str!("fixtures/screen_styled_s2-markdown-link_128x42_1.5.2.txt");
    let (provenance, rows) = fixture.split_once('\n').unwrap();
    assert!(provenance.starts_with("Captured: pi 1.5.2,"));
    let mut capture = String::new();
    for row in rows.lines() {
        capture.push_str(row.strip_suffix('|').expect("fixture row sentinel"));
        capture.push('\n');
    }
    assert_eq!(capture.lines().count(), 42);
    assert_eq!(capture.matches("\x1b[4m").count(), 1);

    let result = observe(&capture);
    assert_eq!(result.facts.input, InputState::Empty);
    assert_eq!(result.facts.turn, TurnState::NotRunning);
    assert_eq!(result.facts.dialog, DialogState::None);
    assert_eq!(result.dialog, ControlDialog::None);
    assert_eq!(result.facts.fresh_editor, FreshEditorEvidence::Unknown);

    // Synthetic: remove only underline, or damage the capture without touching the link.
    assert_eq!(result, observe(&capture.replace("\x1b[4m", "")));
    for damaged in [
        capture.replace("\x1b[4m", "\x1b[8m"),
        capture.replace("\x1b[4m", "\x1b[4:m"),
        capture.replace('─', "x"),
    ] {
        assert_eq!(observe(&damaged), Default::default());
    }
}

#[test]
fn no_speculative_start_or_selector_acceptance() {
    for text in [
        capture!("C3-one-selector-120x40"),
        capture!("R-state-001-120x40"),
        include_str!("fixtures/screen_pi-final-shell_1.5.0.txt"),
    ] {
        assert_eq!(observe(text).dialog, ControlDialog::Unknown);
        assert!(!observe(text).conversation);
    }
    assert_eq!(
        observe(capture!("R-retry-held-120x40")).facts.turn,
        TurnState::Running
    );
}

#[test]
fn synthetic_damaged_active_layouts_and_controls_refuse() {
    let fresh = capture!("B-fresh-120x40");
    for damaged in [
        fresh.replace("0.0%/200k", "unknown footer"),
        fresh.replace('─', "x"),
        format!("{fresh}\x1b[8m"),
        format!("{fresh}\x1b["),
        format!("{fresh}\nunknown footer"),
    ] {
        assert_eq!(observe(&damaged).dialog, ControlDialog::Unknown);
        assert!(!observe(&damaged).conversation);
    }
    // Synthetic: corrupt startup chrome cannot itself count as a conversation.
    let damaged = fresh.replace("Pi can explain its own features", "Unknown startup");
    assert_eq!(
        observe(&damaged).facts.fresh_editor,
        FreshEditorEvidence::Unknown
    );
    assert!(!observe(&damaged).conversation);
}
