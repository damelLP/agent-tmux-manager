use atm_claude_adapter::screen::{self, SCREEN_TEXT_FORMAT};
use atm_core::{
    DialogState, FreshEditorEvidence, InputState, ScreenCursor, ScreenFacts, ScreenInput,
    ScreenTextFormat, TurnState,
};

macro_rules! capture {
    ($name:literal) => {
        include_str!(concat!("fixtures/screen_styled_", $name, "_2.1.325.txt"))
    };
}
const FRESH: &str = capture!("u5b_poor-enabled-fresh");
const DRAFT: &str = capture!("u5b_poor-enabled-same-text-draft-cursor-start");
const BLANK: &str = capture!("u5b_poor-enabled-after-clear-settled");

// Fixtures retain provenance headers, which are not terminal rows.
fn body(input: ScreenInput<'_>) -> ScreenInput<'_> {
    match input {
        ScreenInput::Styled(s) if s.starts_with("Captured:") => {
            ScreenInput::Styled(s.split_once('\n').map_or(s, |(_, body)| body))
        }
        other => other,
    }
}
fn observe_screen(input: ScreenInput<'_>, cursor: Option<ScreenCursor>) -> ScreenFacts {
    screen::observe_screen(body(input), cursor)
}
fn staged_draft_matches(input: ScreenInput<'_>, expected: &str) -> Option<bool> {
    screen::staged_draft_matches(body(input), expected)
}
fn facts(input: InputState, turn: TurnState, fresh: FreshEditorEvidence) -> ScreenFacts {
    let mut f = ScreenFacts::default();
    f.input = input;
    f.turn = turn;
    f.dialog = DialogState::None;
    f.fresh_editor = fresh;
    f
}
fn check(text: &str, expected: ScreenFacts) {
    assert_eq!(SCREEN_TEXT_FORMAT, ScreenTextFormat::Styled);
    for cursor in [
        None,
        Some(ScreenCursor { column: 0, row: 0 }),
        Some(ScreenCursor { column: 2, row: 6 }),
    ] {
        assert_eq!(observe_screen(ScreenInput::Styled(text), cursor), expected);
    }
}

#[test]
fn captured_placeholder_and_home_draft_differ() {
    for text in [
        FRESH,
        capture!("u5b_rich-enabled-fresh"),
        capture!("u5b_screen-with-colorterm-enabled-fresh"),
    ] {
        check(
            text,
            facts(
                InputState::Empty,
                TurnState::NotRunning,
                FreshEditorEvidence::Present,
            ),
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(text), "/clear"),
            None
        );
    }
    for text in [
        DRAFT,
        capture!("u5b_rich-enabled-same-text-draft-cursor-start"),
    ] {
        check(
            text,
            facts(
                InputState::Draft,
                TurnState::NotRunning,
                FreshEditorEvidence::Unknown,
            ),
        );
    }
    assert_eq!(
        staged_draft_matches(
            ScreenInput::Styled(DRAFT),
            "Try \"create a util logging.py that...\""
        ),
        Some(true)
    );
}

#[test]
fn blank_padding_collision_stays_empty_but_staged_leading_spaces_refuse() {
    for text in [BLANK, capture!("u5b_rich-enabled-after-clear-settled")] {
        check(
            text,
            facts(
                InputState::Empty,
                TurnState::NotRunning,
                FreshEditorEvidence::Present,
            ),
        );
    }
    // Synthetic: authored spaces and grid padding are indistinguishable, even at Home.
    let blank = BLANK.replace("\x1b[7m \x1b[0m", "\x1b[7m \x1b[0m     ");
    check(
        &blank,
        facts(
            InputState::Empty,
            TurnState::NotRunning,
            FreshEditorEvidence::Present,
        ),
    );
    for leading in [" ", "     ", "\u{a0}"] {
        let staged = BLANK.replace(
            "\x1b[7m \x1b[0m",
            &format!("{leading}/clear\x1b[7m \x1b[0m     "),
        );
        check(
            &staged,
            facts(
                InputState::Draft,
                TurnState::NotRunning,
                FreshEditorEvidence::Unknown,
            ),
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&staged), "/clear"),
            Some(false)
        );
    }
    // Synthetic: exact unpadded single-line draft; padding-only differences cannot prove exactness.
    let exact = BLANK.replace("\x1b[7m \x1b[0m", "/clear");
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(&exact), "/clear"),
        Some(true)
    );
    assert_eq!(
        staged_draft_matches(
            ScreenInput::Styled(&exact.replace("❯\u{a0}/clear", "❯\u{a0}/clear   ")),
            "/clear"
        ),
        None
    );
    assert_eq!(
        staged_draft_matches(ScreenInput::Plain(&exact), "/clear"),
        None
    );
}

#[test]
fn captured_dialogs_and_unsupported_modes() {
    for text in [
        capture!("p1_A80-default-80x40"),
        capture!("p1_A120-default-120x40"),
        capture!("p1_A200-default-200x40"),
    ] {
        let mut expected = ScreenFacts::default();
        expected.dialog = DialogState::FolderTrust {
            folder: "/tmp/xxxxxxxxxxxxxxxxxxxxxxx/cwd".into(),
        };
        check(text, expected);
    }
    for text in [
        capture!("p1_A80-selected-80x40"),
        capture!("p1_B-clear-007-120x40"),
        capture!("p1_C3-dialog-120x40"),
    ] {
        check(text, ScreenFacts::default());
    }
    // Shell launch line is not startup evidence, even though the auto editor is recognized.
    check(
        capture!("p1_A-auto-ready-120x40"),
        facts(
            InputState::Empty,
            TurnState::Unknown,
            FreshEditorEvidence::Unknown,
        ),
    );
}

#[test]
fn expected_trailing_spaces_cannot_turn_padding_into_exactness() {
    // Synthetic: expected text could accidentally equal the renderer's padding.
    let padded = BLANK.replace("\x1b[7m \x1b[0m", "/clear   ");
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(&padded), "/clear   "),
        None
    );
}

#[test]
fn placeholder_first_cell_requires_exact_intensity() {
    // Synthetic: faint + inverse on T is not the captured placeholder evidence.
    let faint_cursor = FRESH.replace("[7mT", "[2;7mT");
    assert_ne!(
        observe_screen(ScreenInput::Styled(&faint_cursor), None).input,
        InputState::Empty
    );
    check(
        capture!("u5b_rich-enabled-same-text-draft"),
        ScreenFacts::default(),
    );
}

#[test]
fn synthetic_style_and_frame_mutations_never_create_empty() {
    // Synthetic: partial faint, missing inverse, hidden and malformed text, changed/cropped chrome.
    for text in [FRESH.replace("0;2mry", "0mry"), FRESH.replace("[7mT", "[0mT"), format!("{FRESH}\x1b[8m"), format!("{FRESH}\x1b["), FRESH.replace("cycle)", "changed)"), FRESH.replace("────────────────────────────────────────────────────────────────────────────────────────────────────\n", "\n")] {
        assert_ne!(observe_screen(ScreenInput::Styled(&text), None).input, InputState::Empty);
        assert_ne!(staged_draft_matches(ScreenInput::Styled(&text), "/clear"), Some(true));
    }
    let changed = FRESH
        .replace("logging.py", "other.rs")
        .replace("v2.1.325", "v9.8.7")
        .replace("[31m", "[38;2;2;7;0m");
    check(
        &changed,
        facts(
            InputState::Empty,
            TurnState::NotRunning,
            FreshEditorEvidence::Present,
        ),
    );
    let extra = format!("{}\nUnknown footer\n", FRESH.trim_end());
    check(&extra, ScreenFacts::default());
    // Synthetic: multiline draft cannot be reconstructed as a single line.
    let multi = BLANK.replace("\x1b[7m \x1b[0m", "first\n second");
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(&multi), "first\n second"),
        None
    );
    check("\x1b[7m \x1b[0m", ScreenFacts::default());
}
