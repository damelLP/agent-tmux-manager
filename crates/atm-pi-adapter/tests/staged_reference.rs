//! P3 real captures; provenance and source mappings are in fixtures/README.md.
#![cfg(test)]
use atm_core::ScreenInput;
use atm_pi_adapter::screen::staged_draft_matches;

const SHORT: &str = "[m123] Read: atm inbox --id m123";
const LONG: &str = "[m123456] Read: atm inbox --id m123456";
type Capture = (&'static str, &'static str, &'static str, &'static str, bool);
macro_rules! capture {
    ($name:literal, $reference:ident, $matches:literal) => {
        (
            $name,
            include_str!(concat!("fixtures/screen_p3_", $name, "_1.5.0.styled.txt")),
            include_str!(concat!("fixtures/screen_p3_", $name, "_1.5.0.plain.txt")),
            $reference,
            $matches,
        )
    };
}
const CAPTURES: &[Capture] = &[
    capture!("80x40-r0-C-u-empty", SHORT, false),
    capture!("80x40-r0-C-u-staged", SHORT, true),
    capture!("80x40-r0-C-u-after-C-u", SHORT, false),
    capture!("80x40-r0-C-u-cleared", SHORT, false),
    capture!("120x40-r1-C-u-empty", LONG, false),
    capture!("120x40-r1-C-u-staged", LONG, true),
    capture!("120x40-r1-C-u-after-C-u", LONG, false),
    capture!("120x40-r1-C-u-cleared", LONG, false),
    capture!("28x40-r1-C-u-empty", LONG, false),
    capture!("28x40-r1-C-u-staged", LONG, false),
    capture!("28x40-r1-C-u-after-C-u", LONG, false),
    capture!("28x40-r1-C-u-cleared", LONG, false),
    capture!("120x40-r1-Escape-empty", LONG, false),
    capture!("120x40-r1-Escape-staged", LONG, true),
    // One Escape leaves the actual draft intact, not an empty editor.
    capture!("120x40-r1-Escape-after-Escape", LONG, true),
];

const START_LINE: &str = "[m123456] New session: run the shell command atm me";

// Captured: pi 1.5.2, 2026-11-17, sealed stage-only tmux, fail-fast local provider.
// Full provenance and keyboard sequence: fixtures/README.md.
const TR4_CAPTURES: &[(&str, &str, Option<bool>)] = &[
    ("default_80x40_empty_1.5.2", LONG, None),
    ("default_80x40_reference_1.5.2", LONG, Some(true)),
    ("default_80x40_reference-home_1.5.2", LONG, Some(true)),
    ("default_80x40_edited_1.5.2", LONG, Some(false)),
    ("default_80x40_wrong-id_1.5.2", LONG, Some(false)),
    ("default_80x40_wrapped_1.5.2", LONG, None),
    ("default_80x40_start-line_1.5.2", START_LINE, Some(true)),
    ("default_120x40_empty_1.5.2", LONG, None),
    ("default_120x40_reference_1.5.2", LONG, Some(true)),
    ("default_120x40_reference-home_1.5.2", LONG, Some(true)),
    ("default_120x40_edited_1.5.2", LONG, Some(false)),
    ("default_120x40_wrong-id_1.5.2", LONG, Some(false)),
    ("default_120x40_wrapped_1.5.2", LONG, None),
    ("default_120x40_start-line_1.5.2", START_LINE, Some(true)),
    ("default_28x40_empty_1.5.2", LONG, None),
    ("default_28x40_reference_1.5.2", LONG, None),
    ("default_28x40_start-line_1.5.2", START_LINE, None),
];

fn tr4_capture(name: &str, format: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/screen_tr4s0_{name}.{format}.txt"));
    let capture = std::fs::read_to_string(&path);
    assert!(capture.is_ok(), "missing real TR4 S0 capture: {path:?}");
    capture.unwrap()
}

#[test]
fn tr4s0_current_release_exact_reference() {
    // Catches lost positives, substring authorization and weakened closed results.
    for &(name, expected, want) in TR4_CAPTURES {
        let styled = tr4_capture(name, "styled");
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(body(&styled)), expected),
            want,
            "{name}"
        );
        let plain = tr4_capture(name, "plain");
        assert_eq!(
            staged_draft_matches(ScreenInput::Plain(body(&plain)), expected),
            None,
            "{name}: plain"
        );
        if name.contains("_empty_") {
            // Synthetic: history-only text does not create an active draft.
            let history = format!("\x1b[0m{LONG}\n{}", body(&styled));
            assert_eq!(
                staged_draft_matches(ScreenInput::Styled(&history), LONG),
                None,
                "{name}: history with empty editor"
            );
        }
        if name.contains("_wrapped_") {
            let wrapped = format!("{LONG}{}", " neutral human edit".repeat(9));
            assert_eq!(
                staged_draft_matches(ScreenInput::Styled(body(&styled)), &wrapped),
                None,
                "{name}: even the full wrapped literal is not reconstructable"
            );
        }
        if want == Some(true) {
            let other = if expected == LONG { START_LINE } else { LONG };
            assert_eq!(
                staged_draft_matches(ScreenInput::Styled(body(&styled)), other),
                Some(false),
                "{name}: ordinary and start references differ"
            );
        }
    }
}

#[test]
fn tr4s0_human_edits_are_literal_drafts_and_history_cannot_repair_them() {
    for &(name, _, want) in TR4_CAPTURES {
        if !name.contains("_reference_") || want != Some(true) {
            continue;
        }
        let capture = tr4_capture(name, "styled");
        let text = body(&capture);
        assert_eq!(text.matches(LONG).count(), 1, "{name}");
        // Synthetic: change only the final ID, keeping the leading token intact.
        let changed = text.replacen(LONG, "[m123456] Read: atm inbox --id m123459", 1);
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&changed), LONG),
            Some(false),
            "{name}: wrong command ID"
        );
        // Synthetic: the requested reference exists only in history, not the editor.
        let history = format!("\x1b[0m{LONG}\n{changed}");
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&history), LONG),
            Some(false),
            "{name}: history-only"
        );
        // Synthetic: preserve a human's extra leading space, unlike grid padding.
        let leading = text.replacen(LONG, &format!(" {LONG}"), 1);
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&leading), LONG),
            Some(false),
            "{name}: leading space"
        );
        // Synthetic: a suffix is authored content, never part of the reference.
        let edited = format!("{LONG} edited by a human");
        let suffix = text.replacen(LONG, &edited, 1);
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&suffix), LONG),
            Some(false),
            "{name}: suffix"
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&suffix), &edited),
            Some(true),
            "{name}: full literal edited draft"
        );
    }
}

fn body(capture: &str) -> &str {
    let (provenance, text) = capture.split_once('\n').unwrap();
    assert!(provenance.starts_with("Captured: "));
    text
}

#[test]
fn p3_reference_matches_only_the_exact_supported_editor() {
    // Catches lost wide matches and whole-screen/substring authorization.
    for &(name, styled, plain, reference, matches) in CAPTURES {
        let text = body(styled);
        let input = ScreenInput::Styled(text);
        if matches {
            assert_eq!(staged_draft_matches(input, reference), Some(true), "{name}");
        } else {
            // Wrapped/partial drafts are not evidence of the complete reference.
            assert_ne!(staged_draft_matches(input, reference), Some(true), "{name}");
        }
        // Synthetic: a different requested message must never match this editor.
        let wrong = "[m999] Read: atm inbox --id m999";
        assert_ne!(staged_draft_matches(input, wrong), Some(true), "{name}");
        assert_eq!(
            staged_draft_matches(ScreenInput::Plain(body(plain)), reference),
            None,
            "{name}: plain cannot prove a staged draft"
        );
        if !matches {
            continue;
        }

        // Synthetic: change one character in the active editor's message token.
        let token = reference.split_once(' ').unwrap().0;
        assert_eq!(text.matches(token).count(), 1, "{name}: unique token");
        let changed = text.replacen(token, &token.replacen('1', "9", 1), 1);
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&changed), reference),
            Some(false),
            "{name}: changed token"
        );
        // Synthetic: the exact reference in history cannot repair a wrong editor.
        let anchor = "\x1b[2m─";
        assert!(changed.contains(anchor), "{name}: editor border");
        let outside = changed.replacen(anchor, &format!("\x1b[0m{reference}\n\n{anchor}"), 1);
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&outside), reference),
            Some(false),
            "{name}: token outside editor"
        );
    }
}
