//! P5/P6 capture provenance and source mappings are in fixtures/README.md.
#![cfg(test)]
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

macro_rules! p6_capture {
    ($name:literal, $row:literal) => {
        (
            $name,
            include_str!(concat!("fixtures/screen_p6_", $name, "_0.167.0.styled.txt")),
            include_str!(concat!("fixtures/screen_p6_", $name, "_0.167.0.plain.txt")),
            ScreenCursor {
                column: 6,
                row: $row,
            },
        )
    };
}
const P6_CAPTURES: &[Capture] = &[
    p6_capture!("A-80x24-plain-popup", 20),
    p6_capture!("A-80x24-wrap-popup", 20),
    p6_capture!("A-80x24-list-popup", 20),
    p6_capture!("A-80x24-code-popup", 20),
    p6_capture!("A-100x32-plain-popup", 28),
    p6_capture!("A-100x32-wrap-popup", 28),
    p6_capture!("A-100x32-list-popup", 28),
    p6_capture!("A-100x32-code-popup", 28),
    p6_capture!("A-120x40-plain-popup", 36),
    p6_capture!("A-120x40-wrap-popup", 36),
    p6_capture!("A-120x40-list-popup", 36),
    p6_capture!("A-120x40-code-popup", 36),
    p6_capture!("B-80x24-before-enter", 20),
    p6_capture!("B-120x40-before-enter", 36),
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
        for cursor in [None, Some(cursor)] {
            let facts = observe_screen(input, cursor);
            assert_eq!(
                (facts.input, facts.turn, facts.dialog),
                (
                    InputState::Unknown,
                    TurnState::Unknown,
                    DialogState::Unknown
                ),
                "{name}: Plain"
            );
            assert_eq!(
                observe_control_screen(input, cursor).dialog,
                ControlDialog::Unknown,
                "{name}: Plain"
            );
        }
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
    text.lines().find(|line| line.contains("› /new  ")).unwrap()
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
                "only command text not inverse",
                replace_once(text, row, &row.replace("› /new", "›\x1b[27m /new\x1b[7m")),
                true,
            ),
            (
                "only command gap not inverse",
                replace_once(
                    text,
                    row,
                    &row.replace("/new  \x1b[0;7m", "/new\x1b[27m  \x1b[0;7m"),
                ),
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

#[test]
fn p6_captured_finished_popup_is_control_none() {
    for &(name, styled, plain, cursor) in P6_CAPTURES {
        let text = body(styled);
        let input = ScreenInput::Styled(text);
        assert_eq!(staged_draft_matches(input, "/new"), Some(true), "{name}");
        let decoded = StyledScreen::parse(text).unwrap();
        let rows: Vec<_> = decoded.text().lines().collect();
        let status = rows
            .iter()
            .position(|row| row.contains("Worked for "))
            .unwrap();
        assert!(
            status > 0 && rows[status - 1].trim_matches(' ').is_empty(),
            "{name}: blank before status"
        );
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
            assert!(!control.conversation, "{name}");
            assert_eq!(control.dialog, ControlDialog::None, "{name}");
            let plain = ScreenInput::Plain(body(plain));
            let facts = observe_screen(plain, cursor);
            assert_eq!(
                (facts.input, facts.turn, facts.dialog),
                (
                    InputState::Unknown,
                    TurnState::Unknown,
                    DialogState::Unknown
                ),
                "{name}: Plain"
            );
            assert_eq!(
                observe_control_screen(plain, cursor).dialog,
                ControlDialog::Unknown,
                "{name}: Plain"
            );
            assert_eq!(staged_draft_matches(plain, "/new"), None, "{name}: Plain");
        }
    }
}

fn assert_control_unknown(name: &str, label: &str, text: &str, cursor: ScreenCursor) {
    let input = ScreenInput::Styled(text);
    for cursor in [None, Some(cursor)] {
        let control = observe_control_screen(input, cursor);
        assert_eq!(
            control.facts,
            observe_screen(input, cursor),
            "{name}: {label}"
        );
        assert_eq!(control.dialog, ControlDialog::Unknown, "{name}: {label}");
    }
}

#[test]
fn popup_extra_candidates_and_displaced_menu_stay_unknown() {
    for &(name, styled, _, cursor) in CAPTURES.iter().chain(P6_CAPTURES) {
        let text = body(styled);
        let menu = popup_row(text);
        // Synthetic: M2b, M3 and M7 must not authorize a sole selected command.
        for (label, replacement) in [
            (
                "M2b adjacent candidate",
                format!("\x1b[0m  /next  another candidate\n{menu}"),
            ),
            (
                "M3 candidate across blank",
                format!("\x1b[0m  /next  another candidate\n\n{menu}"),
            ),
            (
                "M3 selected candidate across blanks",
                format!("\x1b[1;7m› /next  another candidate\x1b[0m\n\n\n{menu}"),
            ),
            ("M7 extra blank below menu", format!("{menu}\n")),
        ] {
            let changed = replace_once(text, menu, &replacement);
            assert_control_unknown(name, label, &changed, cursor);
        }
    }
}

#[test]
fn p6_status_impostors_and_missing_separator_stay_unknown() {
    for &(name, styled, _, cursor) in P6_CAPTURES {
        let text = body(styled);
        let status = text
            .lines()
            .find(|row| row.contains("Worked for "))
            .unwrap();
        // B-120x40 has blank space above the menu, not a touching status boundary.
        if name == "B-120x40-before-enter" {
            continue;
        }
        // Synthetic: exact chrome, valid values and faint/non-inverse attributes
        // are all required; a candidate cannot masquerade as a finished status.
        for (label, replacement) in [
            ("non-faint status", status.replace("\x1b[2m", "\x1b[0m")),
            ("inverse status", status.replace("\x1b[2m", "\x1b[2;7m")),
            (
                "one non-faint character",
                status.replace("Worked", "W\x1b[22mo\x1b[2mrked"),
            ),
            (
                "one inverse character",
                status.replace("Worked", "W\x1b[7mo\x1b[27mrked"),
            ),
            (
                "candidate prefixed status",
                status.replace("  Worked", "  /next  Worked"),
            ),
            (
                "selection prefixed status",
                status.replace("  Worked", "› Worked"),
            ),
            ("one-space indent", status.replace("  Worked", " Worked")),
            (
                "three-space indent",
                status.replace("  Worked", "   Worked"),
            ),
            ("changed chrome", status.replace("Worked for", "Worked on")),
            ("invalid duration", status.replace("<1s", "0s")),
            ("out-of-range duration", status.replace("<1s", "60s")),
            ("padded duration", status.replace("<1s", "1m 05s")),
            ("invalid clock", status.replace("01:14", "24:00")),
            ("malformed separator", status.replace(" • ", " · ")),
            (
                "extra suffix",
                format!("{} extra", status.trim_end_matches(' ')),
            ),
            (
                "running turn",
                "\x1b[2m  Working (1s • esc to interrupt)".into(),
            ),
            (
                "plain answer touching popup",
                "\x1b[0m  ordinary answer text".into(),
            ),
            (
                "candidate before status",
                format!("\x1b[0m  /next  another candidate\n{status}"),
            ),
            (
                "candidate before separator",
                format!("\x1b[0m  /next  another candidate\n\n{status}"),
            ),
        ] {
            let changed = replace_once(text, status, &replacement);
            assert_control_unknown(name, label, &changed, cursor);
        }
        let mut rows: Vec<_> = text.lines().collect();
        let index = rows.iter().position(|row| *row == status).unwrap();
        rows.remove(index - 1);
        assert_control_unknown(name, "missing pre-status blank", &rows.join("\n"), cursor);
    }
}

#[test]
fn p6_status_values_and_relative_anchors_are_not_literal() {
    for &(name, styled, _, cursor) in P6_CAPTURES {
        let text = body(styled);
        let status = text
            .lines()
            .find(|row| row.contains("Worked for "))
            .unwrap();
        // Synthetic: values use existing finished-duration and clock grammars;
        // hour-duration shapes are not claimed as real popup observations.
        for duration in ["<1s", "7s", "1m 5s", "1h 2m 3s"] {
            for clock in ["00:00", "23:59", "1:00 AM", "12:59 PM"] {
                let replacement = status.replace("<1s", duration).replace("01:14", clock);
                let changed = text.replace(status, &replacement);
                assert_eq!(
                    observe_control_screen(ScreenInput::Styled(&changed), Some(cursor)).dialog,
                    ControlDialog::None,
                    "{name}: {duration}, {clock}"
                );
            }
        }
        let menu = popup_row(text);
        for changed in [
            format!("界 café 🦀 e\u{301}\n\n{text}"),
            text.lines()
                .map(|line| format!("{line}     \n"))
                .collect::<String>(),
            replace_once(text, menu, &format!("\x1b[0m\n\n{menu}")),
        ] {
            let input = ScreenInput::Styled(&changed);
            let control = observe_control_screen(input, None);
            assert_eq!(control.facts, observe_screen(input, None), "{name}");
            assert_eq!(control.dialog, ControlDialog::None, "{name}");
            assert_eq!(staged_draft_matches(input, "/new"), Some(true), "{name}");
        }
    }
}
