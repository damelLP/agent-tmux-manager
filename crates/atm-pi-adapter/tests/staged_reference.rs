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
