//! Decode reuse regressions; capture provenance is in tests/fixtures/README.md.

use super::*;
use std::collections::BTreeMap;
use std::path::Path;

// Frozen decoding/composition from 493a8fb23f8ffcc0069ca5220a2a400b36f264db.
// Only private names and parse-count instrumentation differ. Grammar helpers
// remain shared and unchanged; this is not an independent grammar oracle.
fn base_inspect(
    input: ScreenInput<'_>,
    cursor: Option<ScreenCursor>,
) -> (ScreenFacts, Option<String>) {
    let decoded;
    let (text, styled) = match input {
        ScreenInput::Plain(text) if cursor.is_none() => (text, None),
        ScreenInput::Styled(text) => {
            DECODE_COUNT.with(|count| count.set(count.get() + 1));
            let Ok(screen) = StyledScreen::parse(text) else {
                return Default::default();
            };
            decoded = screen;
            (decoded.text(), Some(&decoded))
        }
        _ => return Default::default(),
    };
    if text.chars().any(|c| c.is_control() && c != '\n') {
        return Default::default();
    }
    let mut lines: Vec<_> = text
        .lines()
        .map(|line| {
            if styled.is_some() {
                line.trim_end_matches(' ')
            } else {
                line
            }
        })
        .collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if known_dialog(&lines) {
        let mut facts = ScreenFacts::default();
        facts.dialog = DialogState::Other;
        return (facts, None);
    }
    editor(&lines, styled).unwrap_or_default()
}

fn base_staged_draft_matches(input: ScreenInput<'_>, expected: &str) -> Option<bool> {
    if !matches!(input, ScreenInput::Styled(_))
        || expected.is_empty()
        || expected.ends_with(char::is_whitespace)
        || expected.chars().any(char::is_control)
    {
        return None;
    }
    let (_, draft) = base_inspect(input, None);
    let draft = draft?;
    Some(draft.trim_end_matches(' ') == expected)
}

fn base_observe_control_screen(
    input: ScreenInput<'_>,
    cursor: Option<ScreenCursor>,
) -> ControlScreen {
    let mut result = ControlScreen::default();
    let decoded;
    let (text, styled) = match input {
        ScreenInput::Plain(text) if cursor.is_none() => (text, None),
        ScreenInput::Styled(text) => {
            DECODE_COUNT.with(|count| count.set(count.get() + 1));
            let Ok(screen) = StyledScreen::parse(text) else {
                return result;
            };
            decoded = screen;
            (decoded.text(), Some(&decoded))
        }
        _ => return result,
    };
    if text.chars().any(|c| c.is_control() && c != '\n') {
        return result;
    }
    let lines: Vec<_> = text.lines().map(|s| s.trim_end_matches(' ')).collect();
    let body = nonblank(&lines);
    result.facts = base_inspect(input, cursor).0;
    if known_dialog(&lines) {
        result.dialog = if update_prompt(&body) {
            ControlDialog::LaunchSkip {
                key: ControlKey::Escape,
            }
        } else {
            ControlDialog::Blocked
        };
    } else if let Some(dialog) = start_dialog(&body).or_else(|| hook_list(&body, text)) {
        result.dialog = dialog;
    } else if result.facts.dialog == DialogState::Other
        && base_staged_draft_matches(input, CLEAR_COMMAND) == Some(true)
        && styled.is_some_and(clear_completion)
    {
        result.dialog = ControlDialog::None;
    } else if result.facts.dialog == DialogState::None {
        result.dialog = ControlDialog::None;
        // Composer comes after the last blank separator and before model/footer.
        // Find its last occurrence; earlier prompts belong to the transcript.
        if let Some(composer) = lines.iter().rposition(|s| s.starts_with("› ")) {
            let history = lines.get(..composer).unwrap_or_default();
            result.conversation = history.iter().any(|s| s.starts_with("› "))
                && history.iter().any(|s| s.starts_with("• "));
        }
    }
    result
}

fn captures() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut captures = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap();
        if name.starts_with("screen") && name.ends_with(".txt") {
            let text = std::fs::read_to_string(&path).unwrap();
            let text = body(&text).to_owned();
            captures.push((name.to_owned(), text));
        }
    }
    captures.sort();
    assert!(!captures.is_empty(), "no screen captures found");
    captures
}

fn body(capture: &str) -> &str {
    if capture.starts_with("Captured:") {
        capture.split_once('\n').unwrap().1
    } else {
        capture
    }
}

#[test]
fn committed_capture_observers_match_base() {
    for (name, text) in captures() {
        for (format, input) in [
            ("Plain", ScreenInput::Plain(&text)),
            ("Styled", ScreenInput::Styled(&text)),
        ] {
            for cursor in [
                None,
                // Synthetic: cursor extremes pin Plain refusal/Styled indifference.
                Some(ScreenCursor { column: 0, row: 0 }),
                Some(ScreenCursor {
                    column: u32::MAX,
                    row: u32::MAX,
                }),
            ] {
                assert_eq!(
                    observe_screen(input, cursor),
                    base_inspect(input, cursor).0,
                    "{name}: {format}, {cursor:?}"
                );
                assert_eq!(
                    observe_control_screen(input, cursor),
                    base_observe_control_screen(input, cursor),
                    "{name}: {format}, {cursor:?}"
                );
            }
            // Synthetic: expected text varies without changing the captured screen.
            for expected in [
                CLEAR_COMMAND,
                "/wrong",
                " /new",
                "Ask Codex to do anything",
                "baseline draft 123",
                "C_EXISTING_DRAFT:",
                "B_EARLY_PASTE",
                "",
                "/new ",
                "/new\u{a0}",
                "\n/new",
                "\t",
                "\0",
            ] {
                assert_eq!(
                    staged_draft_matches(input, expected),
                    base_staged_draft_matches(input, expected),
                    "{name}: {format}, {expected:?}"
                );
            }
        }
    }
}

fn counted<T>(f: impl FnOnce() -> T) -> (T, usize) {
    DECODE_COUNT.with(|count| count.set(0));
    let result = f();
    (result, DECODE_COUNT.with(std::cell::Cell::get))
}

#[test]
fn control_look_decodes_once() {
    let mut groups = BTreeMap::new();
    let mut violations = Vec::new();
    println!("capture\tformat\tbase_parses\tcurrent_parses");
    for (name, text) in captures() {
        for (format, input, expected) in [
            ("Styled", ScreenInput::Styled(&text), 1),
            ("Plain", ScreenInput::Plain(&text), 0),
        ] {
            let (base, before) = counted(|| base_observe_control_screen(input, None));
            let (current, after) = counted(|| observe_control_screen(input, None));
            assert_eq!(current, base, "{name}: {format}");
            println!("{name}\t{format}\t{before}\t{after}");
            *groups.entry((format, before, after)).or_insert(0) += 1;
            if after != expected {
                violations.push((name.clone(), format, after));
            }
        }
    }
    for ((format, before, after), captures) in groups {
        println!("PARSE_COUNTS {format}: {captures} captures, {before} -> {after} per look");
    }
    assert!(violations.is_empty(), "repeated decodes: {violations:?}");
}

#[test]
fn malformed_viewport_refuses_every_entry_point() {
    // Captured: codex-cli 0.167.0, 2026-11-14, U5b/P5 private tmux;
    // methods and sanitization are recorded in tests/fixtures/README.md.
    let fresh = body(include_str!(
        "../../../tests/fixtures/screen_styled_u5b_poor-fresh_0.167.0.txt"
    ));
    let menu = body(include_str!(
        "../../../tests/fixtures/screen_p5_100x40-popup_0.167.0.styled.txt"
    ));
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(menu), CLEAR_COMMAND),
        Some(true)
    );
    for text in [fresh, menu] {
        // Synthetic: remove allocation padding so the legacy Plain layout is
        // supported before corrupting it; retained-space Plain menus refuse.
        let plain = StyledScreen::parse(text)
            .unwrap()
            .text()
            .lines()
            .map(|line| line.trim_end_matches(' '))
            .collect::<Vec<_>>()
            .join("\n");
        for (format, text) in [("Styled", text), ("Plain", plain.as_str())] {
            let input = if format == "Styled" {
                ScreenInput::Styled(text)
            } else {
                ScreenInput::Plain(text)
            };
            assert_ne!(
                observe_screen(input, None),
                ScreenFacts::default(),
                "{format}"
            );
            assert_ne!(
                observe_control_screen(input, None),
                ControlScreen::default(),
                "{format}"
            );
            // Synthetic: the supported frame must not survive malformed escapes,
            // unknown terminal commands, conceal, or any non-newline control.
            for suffix in [
                "\x1b[",
                "\x1b[999m",
                "\x1b[1A",
                "\x1b]8;;https://example.invalid",
                "\x1b]0;title\x07",
                "\x1b[8m",
                "\r",
                "\t",
                "\0",
                "\x7f",
            ] {
                let changed = format!("{text}{suffix}");
                let input = if format == "Styled" {
                    ScreenInput::Styled(&changed)
                } else {
                    ScreenInput::Plain(&changed)
                };
                assert_eq!(
                    observe_screen(input, None),
                    ScreenFacts::default(),
                    "{format}: {suffix:?}"
                );
                assert_eq!(
                    observe_control_screen(input, None),
                    ControlScreen::default(),
                    "{format}: {suffix:?}"
                );
                assert_eq!(
                    staged_draft_matches(input, CLEAR_COMMAND),
                    None,
                    "{format}: {suffix:?}"
                );
            }
        }
    }
}
