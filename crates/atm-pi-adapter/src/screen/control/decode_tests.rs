//! Decode reuse regressions; fixture provenance is in tests/fixtures/README.md.

use super::*;
use std::collections::BTreeMap;
use std::path::Path;

// Frozen decoding/composition from 1279fc0a69c501b14d7ba5b50f80e262c794981c.
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
    result.facts = base_inspect(input, cursor).0;
    if result.facts.dialog != DialogState::None {
        return result;
    }
    result.dialog = ControlDialog::None;
    let decoded;
    let text = match input {
        ScreenInput::Plain(text) => text,
        ScreenInput::Styled(text) => {
            DECODE_COUNT.with(|count| count.set(count.get() + 1));
            let Ok(screen) = StyledScreen::parse(text) else {
                return ControlScreen::default();
            };
            decoded = screen;
            decoded.text()
        }
    };
    let lines: Vec<_> = text.lines().map(|s| s.trim_end_matches(' ')).collect();
    // The existing observer already proved the active editor. Find its upper
    // border (the rule preceding the final rule), excluding the draft itself.
    let Some(lower) = lines.iter().rposition(|s| rule(s)) else {
        return result;
    };
    let Some(history) = lines.get(..lower).and_then(|before| {
        before
            .iter()
            .rposition(|s| s.starts_with("──"))
            .and_then(|top| lines.get(..top))
    }) else {
        return result;
    };
    let startup_end = (0..=history.len())
        .rev()
        .find(|end| history.get(..*end).is_some_and(startup_only));
    result.conversation = if let Some(end) = startup_end {
        history.get(end..).is_some_and(|tail| {
            let blocks: Vec<_> = tail
                .split(|s| s.trim().is_empty())
                .filter(|block| !block.is_empty())
                .collect();
            blocks.len() >= 2 && blocks.iter().all(|block| transcript_block(block))
        })
    } else {
        // P1's working/draft captures have scrolled past the banner. Do not
        // mistake damaged startup chrome for this observed transcript layout.
        let startup_visible = history.iter().any(|s| {
            s.contains("▀▀█")
                || s.contains("escape interrupt ·")
                || s.contains("Press ctrl+o")
                || s.contains("Pi can explain")
        });
        let blocks = history
            .split(|s| s.trim().is_empty())
            .filter(|block| transcript_block(block))
            .count();
        !startup_visible
            && blocks >= 2
            && lines
                .iter()
                .rev()
                .find(|s| !s.is_empty())
                .and_then(|s| usage_footer(s))
                == Some(false)
    };
    result
}

fn captures() -> Vec<(String, String)> {
    fn collect(root: &Path, directory: &Path, captures: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(root, &path, captures);
            } else if path.extension().is_some_and(|extension| extension == "txt") {
                let text = std::fs::read_to_string(&path).unwrap();
                let name = path.strip_prefix(root).unwrap().display().to_string();
                captures.push((name, body(&text).to_owned()));
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut captures = Vec::new();
    collect(&root, &root, &mut captures);
    captures.sort();
    assert!(!captures.is_empty(), "no screen fixtures found");
    captures
}

fn body(capture: &str) -> &str {
    if capture.starts_with("Captured:") || capture.starts_with("Synthetic:") {
        capture.split_once('\n').unwrap().1
    } else {
        capture
    }
}

// Synthetic: cursor extremes pin Plain refusal and Styled indifference.
const CURSORS: [Option<ScreenCursor>; 3] = [
    None,
    Some(ScreenCursor { column: 0, row: 0 }),
    Some(ScreenCursor {
        column: u32::MAX,
        row: u32::MAX,
    }),
];

#[test]
fn committed_capture_observers_match_base() {
    for (name, text) in captures() {
        for (format, input) in [
            ("Plain", ScreenInput::Plain(&text)),
            ("Styled", ScreenInput::Styled(&text)),
        ] {
            for cursor in CURSORS {
                let base = base_inspect(input, cursor);
                assert_eq!(inspect(input, cursor), base, "{name}: {format}, {cursor:?}");
                assert_eq!(
                    observe_screen(input, cursor),
                    base.0,
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
                "[m123] Read: atm inbox --id m123",
                "[m123456] Read: atm inbox --id m123456",
                " café 界",
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
    println!("capture\tformat\tcursor\tbase_parses\tcurrent_parses");
    for (name, text) in captures() {
        for (format, input, expected) in [
            ("Styled", ScreenInput::Styled(&text), 1),
            ("Plain", ScreenInput::Plain(&text), 0),
        ] {
            for (cursor_name, cursor) in ["none", "home", "max"].into_iter().zip(CURSORS) {
                let (base, before) = counted(|| base_observe_control_screen(input, cursor));
                let (current, after) = counted(|| observe_control_screen(input, cursor));
                assert_eq!(current, base, "{name}: {format}, {cursor:?}");
                println!("{name}\t{format}\t{cursor_name}\t{before}\t{after}");
                *groups
                    .entry((format, cursor_name, before, after))
                    .or_insert(0) += 1;
                if after != expected {
                    violations.push((name.clone(), format, cursor_name, after));
                }
            }
        }
    }
    for ((format, cursor, before, after), captures) in groups {
        println!(
            "PARSE_COUNTS {format}/{cursor}: {captures} fixtures, {before} -> {after} per look"
        );
    }
    assert!(violations.is_empty(), "repeated decodes: {violations:?}");
}

#[test]
fn malformed_viewport_refuses_every_entry_point() {
    // Captured: pi 1.5.0, 2026-11-14, P1/P5 sealed private tmux;
    // methods and sanitization are recorded in tests/fixtures/README.md.
    let fresh = body(include_str!(
        "../../../tests/fixtures/screen_styled_p1_B-fresh-120x40_1.5.0.txt"
    ));
    let staged = body(include_str!(
        "../../../tests/fixtures/screen_p5_120x40-staged_1.5.0.styled.txt"
    ));
    assert_eq!(
        observe_screen(ScreenInput::Styled(fresh), None).fresh_editor,
        FreshEditorEvidence::Present
    );
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(staged), CLEAR_COMMAND),
        Some(true)
    );
    for text in [fresh, staged] {
        // Synthetic: remove retained grid padding for a supported legacy Plain
        // layout before corruption; the full corpus above retains every byte.
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
            // Synthetic: no supported frame survives malformed/unsupported
            // escapes, conceal, or non-newline controls at any public entry point.
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
                for (position, changed) in [
                    ("before", format!("{suffix}{text}")),
                    ("after", format!("{text}{suffix}")),
                ] {
                    let input = if format == "Styled" {
                        ScreenInput::Styled(&changed)
                    } else {
                        ScreenInput::Plain(&changed)
                    };
                    for cursor in CURSORS {
                        let base = base_observe_control_screen(input, cursor);
                        assert_eq!(
                            base,
                            ControlScreen::default(),
                            "{format}/{position}: {suffix:?}, {cursor:?}"
                        );
                        assert_eq!(
                            observe_control_screen(input, cursor),
                            base,
                            "{format}/{position}: {suffix:?}, {cursor:?}"
                        );
                        assert_eq!(
                            observe_screen(input, cursor),
                            ScreenFacts::default(),
                            "{format}/{position}: {suffix:?}, {cursor:?}"
                        );
                    }
                    assert_eq!(
                        staged_draft_matches(input, CLEAR_COMMAND),
                        base_staged_draft_matches(input, CLEAR_COMMAND),
                        "{format}/{position}: {suffix:?}"
                    );
                    assert_eq!(
                        staged_draft_matches(input, CLEAR_COMMAND),
                        None,
                        "{format}/{position}: {suffix:?}"
                    );
                }
            }
        }
    }
}
