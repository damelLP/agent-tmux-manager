use atm_codex_adapter::screen::{self, SCREEN_TEXT_FORMAT};
use atm_core::{
    DialogState, FreshEditorEvidence, InputState, ScreenCursor, ScreenFacts, ScreenInput,
    ScreenTextFormat, TurnState,
};
macro_rules! capture {
    ($name:literal) => {
        include_str!(concat!("fixtures/screen_styled_", $name, "_0.167.0.txt"))
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
        Some(ScreenCursor { column: 0, row: 0 }),
        Some(ScreenCursor { column: 2, row: 26 }),
    ] {
        assert_eq!(
            screen::observe_screen(ScreenInput::Styled(text), cursor),
            expected
        );
    }
}
#[test]
fn captured_modes_placeholders_and_home_drafts() {
    use FreshEditorEvidence::{Present, Unknown};
    use InputState::{Draft, Empty};
    use TurnState::NotRunning;
    for text in [
        capture!("u5b_poor-fresh"),
        capture!("u5b_rich-fresh"),
        capture!("u5b_screen-with-colorterm-fresh"),
        capture!("p1_b37-poor-baseline-100x24"),
        capture!("p1_b14-fresh-baseline-32-100x32"),
    ] {
        check(text, facts(Empty, NotRunning, DialogState::None, Present));
        assert_eq!(
            screen::staged_draft_matches(ScreenInput::Styled(text), "Ask Codex to do anything"),
            None
        );
    }
    for text in [
        capture!("u5b_poor-same-text-draft-cursor-start"),
        capture!("u5b_rich-same-text-draft-cursor-start"),
        capture!("u5b_screen-with-colorterm-same-text-draft-cursor-start"),
    ] {
        check(text, facts(Draft, NotRunning, DialogState::None, Unknown));
        assert_eq!(
            screen::staged_draft_matches(ScreenInput::Styled(text), "Ask Codex to do anything"),
            Some(true)
        );
    }
    for text in [
        capture!("u5b_poor-different-draft"),
        capture!("u5b_rich-different-draft"),
        capture!("u5b_screen-with-colorterm-different-draft"),
    ] {
        check(text, facts(Draft, NotRunning, DialogState::None, Unknown));
        assert_eq!(
            screen::staged_draft_matches(ScreenInput::Styled(text), "baseline draft 123"),
            Some(true)
        );
    }
    check(
        capture!("p1_c03-draft-before-100x24"),
        facts(Draft, TurnState::Unknown, DialogState::None, Unknown),
    );
    check(
        capture!("p1_c07-working-held-100x24"),
        facts(Empty, TurnState::Running, DialogState::None, Unknown),
    );
    check(
        capture!("p1_b05-repeat-new-draft-100x32"),
        facts(Draft, TurnState::Unknown, DialogState::Other, Unknown),
    );
    // Captured multiline text is already history, not the active composer.
    check(
        capture!("p1_c02-multiline-100x24"),
        facts(Empty, TurnState::Unknown, DialogState::None, Unknown),
    );
}
#[test]
fn captured_unsafe_dialogs_and_multiline() {
    for text in [
        capture!("p1_a16-default-fresh-folder-trust-100x24"),
        capture!("p1_a17-default-fresh-hook-trust-100x24"),
        capture!("p1_b31-nodaemon-clear-dialog-100x32"),
        capture!("p1_c16-permission-visible-100x32"),
    ] {
        check(text, ScreenFacts::default());
        assert_eq!(
            screen::staged_draft_matches(ScreenInput::Styled(text), "/new"),
            None
        );
    }
}
#[test]
fn partial_faint_placeholder_and_missing_chrome_never_empty() {
    let fresh = capture!("u5b_poor-fresh");
    // Synthetic: style changes must not manufacture faint proof from colors or a partial run.
    for text in [
        fresh.replace("[2mAsk", "[38;2;2;7;0mAsk"),
        fresh.replace("Ask Codex", "Ask\x1b[22m Codex"),
        fresh.replace("anything", "anything\x1b[8m"),
        format!("{fresh}\x1b["),
        fresh
            .lines()
            .filter(|l| !l.contains("shortcuts"))
            .collect::<Vec<_>>()
            .join("\n"),
        fresh
            .lines()
            .filter(|l| !l.contains("default"))
            .collect::<Vec<_>>()
            .join("\n"),
        fresh.replace("›", ">"),
        fresh.replace("Ask Codex to do anything", ""),
        fresh.replace("Ask Codex to do anything", "first\nsecond"),
    ] {
        assert_ne!(
            screen::observe_screen(ScreenInput::Styled(&text), None).input,
            InputState::Empty
        );
    }
    check("\x1b[7m \x1b[0m", ScreenFacts::default());
    // Synthetic: values vary while exact help chrome remains.
    let changed = capture!("p1_b37-poor-baseline-100x24")
        .replace("1 warning", "23 warnings")
        .replace("v0.167.0", "v9.2.1")
        .replace("gpt-5.4", "other-model");
    check(
        &changed,
        facts(
            InputState::Empty,
            TurnState::NotRunning,
            DialogState::None,
            FreshEditorEvidence::Present,
        ),
    );
}
#[test]
fn expected_trailing_spaces_cannot_turn_padding_into_exactness() {
    // Synthetic: expected text could accidentally equal the renderer's padding.
    let padded = capture!("u5b_poor-different-draft").replace("baseline draft 123", "/clear   ");
    assert_eq!(
        screen::staged_draft_matches(ScreenInput::Styled(&padded), "/clear   "),
        None
    );
}

#[test]
fn leading_spaces_after_blank_draft_paste_never_match_command() {
    // Synthetic: whitespace-only draft plus paste; compare exact leading whitespace.
    let draft = capture!("u5b_poor-different-draft");
    for leading in [" ", "     ", "\u{a0}"] {
        let text = draft.replace("baseline draft 123", &format!("{leading}/clear   "));
        assert_eq!(
            screen::staged_draft_matches(ScreenInput::Styled(&text), "/clear"),
            Some(false)
        );
        assert_eq!(
            screen::observe_screen(ScreenInput::Styled(&text), None).input,
            InputState::Draft
        );
    }
    let exact = draft.replace("baseline draft 123", "/clear");
    assert_eq!(
        screen::staged_draft_matches(ScreenInput::Styled(&exact), "/clear"),
        Some(true)
    );
    let padded = draft.replace("baseline draft 123", "/clear   ");
    assert_eq!(
        screen::staged_draft_matches(ScreenInput::Styled(&padded), "/clear"),
        None
    );
    assert_eq!(
        screen::staged_draft_matches(ScreenInput::Plain(&exact), "/clear"),
        None
    );
    assert_eq!(
        screen::staged_draft_matches(ScreenInput::Styled(&exact), "a\nb"),
        None
    );
}
