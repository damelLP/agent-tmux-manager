//! P3 real captures; provenance and source mappings are in fixtures/README.md.
#![cfg(test)]
use atm_codex_adapter::screen::staged_draft_matches;
use atm_core::ScreenInput;

const SHORT: &str = "[m123] Read: atm inbox --id m123";
const LONG: &str = "[m123456] Read: atm inbox --id m123456";
type Capture = (&'static str, &'static str, &'static str, &'static str, bool);
macro_rules! capture {
    ($name:literal, $reference:ident, $matches:literal) => {
        (
            $name,
            include_str!(concat!("fixtures/screen_p3_", $name, "_0.167.0.styled.txt")),
            include_str!(concat!("fixtures/screen_p3_", $name, "_0.167.0.plain.txt")),
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

#[test]
fn tr4s0_existing_captures_pin_exact_tristate_results() {
    // Replay only: retain the P3 capture version/date, never claim a new live run.
    // Catches a wrapped/empty fallback becoming positive or a wide match being lost.
    let wants = [
        None,
        Some(true),
        None,
        None,
        None,
        Some(true),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(true),
        Some(true),
    ];
    assert_eq!(CAPTURES.len(), wants.len());
    for (&(name, styled, plain, reference, _), want) in CAPTURES.iter().zip(wants) {
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(body(styled)), reference),
            want,
            "{name}"
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Plain(body(plain)), reference),
            None,
            "{name}: plain"
        );
    }
}

#[test]
fn tr4s0_synthetic_start_lines_and_edits_stay_exact_in_existing_frames() {
    const START: &str = "[m123456] New session: run the shell command atm me";
    for &(name, styled, _, reference, matches) in CAPTURES {
        if !matches {
            continue;
        }
        let text = body(styled);
        assert_eq!(text.matches(reference).count(), 1, "{name}");
        // Synthetic: stage the start-line literal in an existing wide input frame;
        // this is not a captured Codex start-line session or Enter authorization.
        let start = text.replacen(reference, START, 1);
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&start), START),
            Some(true),
            "{name}: literal start line"
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&start), reference),
            Some(false),
            "{name}: ordinary reference differs"
        );
        // Synthetic: neither a human suffix nor a leading space is grid padding.
        for edited in [format!(" {reference}"), format!("{reference} edited")] {
            let draft = text.replacen(reference, &edited, 1);
            assert_eq!(
                staged_draft_matches(ScreenInput::Styled(&draft), reference),
                Some(false),
                "{name}: edited reference"
            );
            assert_eq!(
                staged_draft_matches(ScreenInput::Styled(&draft), &edited),
                Some(true),
                "{name}: whole authored literal"
            );
        }
        // Synthetic: retain the leading ID but edit the command's final ID only.
        let wrong = reference.replacen("--id m1", "--id m9", 1);
        assert_ne!(wrong, reference, "{name}: effective mutation");
        let changed = text.replacen(reference, &wrong, 1);
        // Synthetic: put the exact requested reference only in transcript history.
        let history = format!("\x1b[0m{reference}\n{changed}");
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&history), reference),
            Some(false),
            "{name}: history-only"
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
        let anchor = "\x1b[1m›";
        assert!(changed.contains(anchor), "{name}: editor prompt");
        let outside = changed.replacen(anchor, &format!("\x1b[0m{reference}\n\n{anchor}"), 1);
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&outside), reference),
            Some(false),
            "{name}: token outside editor"
        );
    }
}
