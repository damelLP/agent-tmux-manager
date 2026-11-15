//! P5 capture provenance and source mappings are in fixtures/README.md.
use atm_codex_adapter::screen::{observe_control_screen, observe_screen, staged_draft_matches};
use atm_core::{
    DialogState, FreshEditorEvidence, InputState, ScreenCursor, ScreenInput, TurnState,
};

// A capture includes its actual cursor metadata, not inferred coordinates.
type Capture = (&'static str, &'static str, &'static str, ScreenCursor);
macro_rules! capture {
    ($name:literal, $column:literal, $row:literal) => {
        (
            $name,
            include_str!(concat!("fixtures/screen_p5_", $name, "_0.167.0.styled.txt")),
            include_str!(concat!("fixtures/screen_p5_", $name, "_0.167.0.plain.txt")),
            ScreenCursor {
                column: $column,
                row: $row,
            },
        )
    };
}
const CAPTURES: &[Capture] = &[
    capture!("80x40-staged", 6, 36),
    capture!("80x40-popup", 6, 36),
    capture!("80x40-first-after-down", 6, 36),
    capture!("100x40-staged", 6, 36),
    capture!("100x40-popup", 6, 36),
    capture!("100x40-first-after-down", 6, 36),
    capture!("120x40-staged", 6, 36),
    capture!("120x40-popup", 6, 36),
    capture!("120x40-first-after-down", 6, 36),
];

fn body(capture: &str) -> &str {
    let (provenance, text) = capture.split_once('\n').unwrap();
    assert!(provenance.starts_with("Captured: "));
    text
}

#[test]
fn p5_staged_clear_matches_exactly() {
    for &(name, styled, plain, cursor) in CAPTURES {
        let text = body(styled);
        let input = ScreenInput::Styled(text);
        assert_eq!(staged_draft_matches(input, "/new"), Some(true), "{name}");
        // Synthetic expected-text changes catch prefix/substring matching.
        for expected in ["/nex", "/newx", "/new arg"] {
            assert_eq!(
                staged_draft_matches(input, expected),
                Some(false),
                "{name}: {expected}"
            );
        }
        for cursor in [None, Some(cursor)] {
            let facts = observe_screen(input, cursor);
            let control = observe_control_screen(input, cursor);
            assert_eq!(control.facts, facts, "{name}");
            assert_eq!(
                (facts.input, facts.turn, facts.dialog),
                (InputState::Draft, TurnState::NotRunning, DialogState::Other),
                "{name}"
            );
            assert_eq!(facts.fresh_editor, FreshEditorEvidence::Unknown, "{name}");
        }
        let input = ScreenInput::Plain(body(plain));
        assert_eq!(staged_draft_matches(input, "/new"), None, "{name}");
        assert_eq!(
            observe_screen(input, Some(cursor)).input,
            InputState::Unknown,
            "{name}"
        );
    }
}

fn changed_editor(text: &str, replacement: &str) -> String {
    let needle = "›\x1b[0m /new";
    assert_eq!(
        text.matches(needle).count(),
        1,
        "unique active editor anchor"
    );
    let changed = text.replace(needle, &needle.replace("/new", replacement));
    assert_ne!(changed, text);
    changed
}

#[test]
fn p5_mutated_editor_never_matches() {
    for &(name, styled, _, _) in CAPTURES {
        let text = body(styled);
        // Synthetic: mutate only the active editor, never its completion row.
        for replacement in ["/nex", "/newx", "/new arg", " /new", "wrong\n/new"] {
            let changed = changed_editor(text, replacement);
            assert_ne!(
                staged_draft_matches(ScreenInput::Styled(&changed), "/new"),
                Some(true),
                "{name}: {replacement:?}"
            );
        }
        // Synthetic: correct text in history cannot override the wrong editor.
        let changed = format!("/new\n{}", changed_editor(text, "/wrong"));
        assert_ne!(
            staged_draft_matches(ScreenInput::Styled(&changed), "/new"),
            Some(true),
            "{name}: history"
        );
    }
}
