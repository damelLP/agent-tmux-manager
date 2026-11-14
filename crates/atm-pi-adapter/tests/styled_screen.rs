use atm_core::{
    DialogState, FreshEditorEvidence, InputState, ScreenCursor, ScreenFacts, ScreenInput,
    ScreenTextFormat, TurnState,
};
use atm_pi_adapter::screen::{self, SCREEN_TEXT_FORMAT};
macro_rules! capture {
    ($name:literal) => {
        include_str!(concat!("fixtures/screen_styled_p1_", $name, "_1.5.0.txt"))
            .split_once('\n')
            .unwrap()
            .1
    };
}
fn facts(
    input: InputState,
    turn: TurnState,
    dialog: DialogState,
    fresh: FreshEditorEvidence,
) -> ScreenFacts {
    let mut f = ScreenFacts::default();
    f.input = input;
    f.turn = turn;
    f.dialog = dialog;
    f.fresh_editor = fresh;
    f
}
fn check(text: &str, expected: ScreenFacts) {
    assert_eq!(SCREEN_TEXT_FORMAT, ScreenTextFormat::Styled);
    for cursor in [
        None,
        Some(ScreenCursor { column: 0, row: 36 }),
        Some(ScreenCursor { column: 8, row: 36 }),
    ] {
        assert_eq!(
            screen::observe_screen(ScreenInput::Styled(text), cursor),
            expected
        );
    }
}
#[test]
fn captured_blank_padding_collision_stays_empty() {
    for text in [capture!("A-ready-120x40"), capture!("B-fresh-120x40")] {
        check(
            text,
            facts(
                InputState::Empty,
                TurnState::NotRunning,
                DialogState::None,
                FreshEditorEvidence::Present,
            ),
        );
        assert_eq!(
            screen::staged_draft_matches(ScreenInput::Styled(text), "/new"),
            None
        );
    }
}
#[test]
fn captured_single_and_multiline_drafts_and_unsafe_states() {
    for text in [
        capture!("C4-one-draft-120x40"),
        capture!("C1-multi-pasted-120x40"),
    ] {
        check(
            text,
            facts(
                InputState::Draft,
                TurnState::NotRunning,
                DialogState::None,
                FreshEditorEvidence::Unknown,
            ),
        );
    }
    assert_eq!(
        screen::staged_draft_matches(
            ScreenInput::Styled(capture!("C1-multi-pasted-120x40")),
            "C1_FIRST \"double\"\n\nC1_THIRD 'single' café"
        ),
        None
    );
    for text in [
        capture!("C2-steer-one-held-120x40"),
        capture!("C2-steer-one-queued-120x40"),
    ] {
        check(
            text,
            facts(
                InputState::Empty,
                TurnState::Running,
                DialogState::None,
                FreshEditorEvidence::Unknown,
            ),
        );
    }
    check(
        capture!("C3-one-selector-120x40"),
        facts(
            InputState::Unknown,
            TurnState::Unknown,
            DialogState::Other,
            FreshEditorEvidence::Unknown,
        ),
    );
    check(
        capture!("R-retry-held-120x40"),
        facts(
            InputState::Empty,
            TurnState::Running,
            DialogState::None,
            FreshEditorEvidence::Unknown,
        ),
    );
    check(capture!("R-state-001-120x40"), ScreenFacts::default());
}
#[test]
fn whitespace_only_draft_plus_paste_preserves_leading_spaces() {
    let blank = capture!("B-fresh-120x40");
    // Synthetic: Home cannot distinguish authored spaces from allocated padding.
    let spaces = blank.replace("\x1b[0;7m \x1b[0m", "\x1b[0;7m \x1b[0m   ");
    check(
        &spaces,
        facts(
            InputState::Empty,
            TurnState::NotRunning,
            DialogState::None,
            FreshEditorEvidence::Present,
        ),
    );
    for leading in [" ", "    ", "\u{a0}"] {
        let staged = blank.replace(
            "\x1b[0;7m \x1b[0m",
            &format!("\x1b[0m{leading}/clear\x1b[7m \x1b[0m"),
        );
        check(
            &staged,
            facts(
                InputState::Draft,
                TurnState::NotRunning,
                DialogState::None,
                FreshEditorEvidence::Unknown,
            ),
        );
        assert_eq!(
            screen::staged_draft_matches(ScreenInput::Styled(&staged), "/clear"),
            Some(false)
        );
    }
    // Synthetic: unpadded visible row vs. padding ambiguity; leading spaces are never trimmed.
    let exact = blank
        .lines()
        .map(|line| {
            if line.contains("\x1b[0;7m \x1b[0m") {
                "\x1b[0m/clear"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        screen::staged_draft_matches(ScreenInput::Styled(&exact), "/clear"),
        Some(true)
    );
    assert_eq!(
        screen::staged_draft_matches(
            ScreenInput::Styled(&exact.replace("/clear", "/clear   ")),
            "/clear"
        ),
        None
    );
    assert_eq!(
        screen::staged_draft_matches(ScreenInput::Plain(&exact), "/clear"),
        None
    );
    let unicode = exact.replace("/clear", " café 界");
    assert_eq!(
        screen::staged_draft_matches(ScreenInput::Styled(&unicode), " café 界"),
        Some(true)
    );
    assert_eq!(
        screen::staged_draft_matches(ScreenInput::Styled(&unicode), "café 界"),
        Some(false)
    );
}
#[test]
fn synthetic_unsafe_chrome_and_controls_refuse() {
    let blank = capture!("B-fresh-120x40");
    for text in [
        format!("{blank}\x1b[8m"),
        format!("{blank}\x1b["),
        blank.replace("0.0%/200k", "unknown footer"),
        capture!("C2-steer-one-held-120x40").replace("Working", "Other"),
    ] {
        check(&text, ScreenFacts::default());
        assert_eq!(
            screen::staged_draft_matches(ScreenInput::Styled(&text), "/clear"),
            None
        );
    }
    // Synthetic: nested pasted border after conversation cannot be an empty active editor.
    let draft = capture!("C4-one-draft-120x40");
    let pasted = draft.replace("C4_DRAFT:", &format!("\n{}\n", "─".repeat(120)));
    check(&pasted, ScreenFacts::default());
    check("\x1b[7m \x1b[0m", ScreenFacts::default());
}
