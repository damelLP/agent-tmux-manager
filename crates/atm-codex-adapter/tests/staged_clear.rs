//! P5 capture provenance and source mappings are in fixtures/README.md.
use atm_codex_adapter::screen::{observe_control_screen, observe_screen, staged_draft_matches};
use atm_core::harness_control::ControlDialog;
use atm_core::styled_screen::StyledScreen;
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

#[test]
fn p5_codex_single_new_popup_is_control_none() {
    for &(name, styled, _, cursor) in CAPTURES {
        let input = ScreenInput::Styled(body(styled));
        assert_eq!(staged_draft_matches(input, "/new"), Some(true), "{name}");
        for cursor in [None, Some(cursor)] {
            let facts = observe_screen(input, cursor);
            let control = observe_control_screen(input, cursor);
            assert_eq!(control.facts, facts, "{name}");
            assert_eq!(
                (facts.input, facts.turn, facts.dialog),
                (InputState::Draft, TurnState::NotRunning, DialogState::Other),
                "{name}"
            );
            assert_eq!(control.dialog, ControlDialog::None, "{name}");
        }
    }
}

fn replace_once(text: &str, old: &str, new: &str) -> String {
    assert_eq!(
        text.matches(old).count(),
        1,
        "unique mutation anchor: {old:?}"
    );
    let changed = text.replace(old, new);
    assert_ne!(changed, text);
    changed
}

fn popup_row(text: &str) -> &str {
    text.lines()
        .find(|line| line.starts_with("\x1b[1;7m› /new  "))
        .unwrap()
}

#[test]
fn p5_codex_popup_mutations_stay_unknown() {
    for &(name, styled, plain, cursor) in CAPTURES {
        let text = body(styled);
        let row = popup_row(text);
        let (above, below) = text.split_once(row).unwrap();
        let blank = below
            .strip_prefix('\n')
            .unwrap()
            .split_once('\n')
            .unwrap()
            .0;
        let footer = text
            .lines()
            .find(|line| line.contains("gpt-5.4 default"))
            .unwrap();
        let unselected = row
            .replace("\x1b[1;7m", "\x1b[1m")
            .replace("\x1b[0;7m", "\x1b[0m");
        let other = "\x1b[0m  /next  another candidate";
        // Synthetic: each changed screen isolates a missing safety condition.
        // The bool marks mutations where delivery must still be Other.
        let mutations = [
            (
                "other command selected",
                replace_once(text, row, &row.replace("/new", "/next")),
                false,
            ),
            (
                "selection moved",
                replace_once(
                    text,
                    row,
                    &format!("{unselected}\n\x1b[1;7m› /next  another candidate\x1b[0m"),
                ),
                false,
            ),
            (
                "candidate below",
                replace_once(text, row, &format!("{row}\n{other}")),
                false,
            ),
            (
                "candidate above",
                replace_once(text, row, &format!("{other}\n{row}")),
                true,
            ),
            (
                "second selected candidate",
                replace_once(text, row, &format!("{row}\n{row}")),
                true,
            ),
            (
                "missing arrow",
                replace_once(text, row, &row.replacen('›', " ", 1)),
                false,
            ),
            ("no inverse", replace_once(text, row, &unselected), true),
            (
                "command not inverse",
                replace_once(text, row, &row.replace("\x1b[1;7m", "\x1b[1m")),
                true,
            ),
            (
                "description not inverse",
                replace_once(text, row, &row.replace("\x1b[0;7m", "\x1b[0m")),
                true,
            ),
            (
                "arrow alone inverse",
                replace_once(
                    text,
                    row,
                    &row.replace("› /new", "›\x1b[27m /new")
                        .replace("\x1b[0;7m", "\x1b[0m"),
                ),
                true,
            ),
            (
                "changed description",
                replace_once(text, "start a new chat", "start a fresh chat"),
                false,
            ),
            (
                "changed description column",
                replace_once(text, row, &row.replace("/new  ", "/new   ")),
                false,
            ),
            (
                "argument hint",
                replace_once(text, row, &row.replace("/new  ", "/new [name]  ")),
                false,
            ),
            (
                "missing composer separator",
                replace_once(text, &format!("{row}\n{blank}\n"), &format!("{row}\n")),
                false,
            ),
            (
                "missing upper boundary",
                format!("{}\n{row}{below}", above.trim_end()),
                true,
            ),
            (
                "damaged footer",
                replace_once(text, footer, "unsupported footer"),
                false,
            ),
            ("truncated footer", replace_once(text, footer, ""), false),
            (
                "faint draft",
                replace_once(text, "›\x1b[0m /new", "›\x1b[0;2m /new"),
                true,
            ),
            ("unsupported escape", format!("\x1b[2J{text}"), false),
        ];
        for (label, changed, delivery_other) in mutations {
            assert_ne!(changed, text, "{label}");
            let input = ScreenInput::Styled(&changed);
            for cursor in [None, Some(cursor)] {
                let facts = observe_screen(input, cursor);
                let control = observe_control_screen(input, cursor);
                assert_eq!(control.facts, facts, "{name}: {label}");
                assert_eq!(control.dialog, ControlDialog::Unknown, "{name}: {label}");
                if delivery_other {
                    assert_eq!(facts.dialog, DialogState::Other, "{name}: {label}");
                }
            }
        }
        let decoded = StyledScreen::parse(text).unwrap();
        for input in [
            ScreenInput::Plain(body(plain)),
            ScreenInput::Plain(decoded.text()),
            ScreenInput::Styled(decoded.text()),
        ] {
            for cursor in [None, Some(cursor)] {
                assert_eq!(
                    observe_control_screen(input, cursor).dialog,
                    ControlDialog::Unknown,
                    "{name}: no selection attributes"
                );
            }
        }
    }
}

#[test]
fn p5_codex_popup_is_anchor_based() {
    for &(name, styled, _, _) in CAPTURES {
        let text = body(styled);
        let row = popup_row(text);
        let (above, below) = text.split_once(row).unwrap();
        // Synthetic: change vertical position, grid padding and footer values,
        // keeping the captured single-row menu and its boundaries intact.
        let compact = format!("{}\n\n{row}{below}", above.trim_end());
        let padded = text
            .lines()
            .map(|line| format!("{line}     \n"))
            .collect::<String>();
        let varied = text.replace("gpt-5.4 default", "model-z extra-high");
        let decoded = StyledScreen::parse(text).unwrap();
        let cwd = decoded
            .text()
            .lines()
            .find_map(|line| line.trim().strip_prefix("gpt-5.4 default · "))
            .unwrap()
            .trim_end();
        let relocated = replace_once(
            &varied,
            &format!("{cwd}\x1b[39m"),
            "/tmp/another-project\x1b[39m",
        );
        for changed in [
            format!("\n\n{text}"),
            compact,
            padded,
            relocated,
            format!("界 café\n{text}"),
        ] {
            assert_ne!(changed, text);
            let input = ScreenInput::Styled(&changed);
            let facts = observe_screen(input, None);
            let control = observe_control_screen(input, None);
            assert_eq!(control.facts, facts, "{name}");
            assert_eq!(facts.dialog, DialogState::Other, "{name}");
            assert_eq!(control.dialog, ControlDialog::None, "{name}");
            assert_eq!(staged_draft_matches(input, "/new"), Some(true), "{name}");
            let row = popup_row(&changed);
            let extra = replace_once(
                &changed,
                row,
                &format!("\x1b[0m  /next  another candidate\n{row}"),
            );
            assert_eq!(
                observe_control_screen(ScreenInput::Styled(&extra), None).dialog,
                ControlDialog::Unknown,
                "{name}: shifted second candidate"
            );
        }
    }
}
