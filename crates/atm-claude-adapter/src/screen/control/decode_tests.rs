//! Decode reuse regressions; capture provenance is in tests/fixtures/README.md.

use super::*;
use std::collections::BTreeMap;
use std::path::Path;

// Frozen decoding/composition from 1279fc0a69c501b14d7ba5b50f80e262c794981c.
// Only private names and parse-count instrumentation differ. Grammar helpers
// remain shared and unchanged; this is not an independent grammar oracle.
fn base_observe_screen(input: ScreenInput<'_>, cursor: Option<ScreenCursor>) -> ScreenFacts {
    base_inspect(input, cursor).0
}

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
    if let Some(dialog) = dialog(&lines) {
        let mut facts = ScreenFacts::default();
        facts.dialog = dialog;
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
    if let ScreenInput::Styled(text) = input {
        DECODE_COUNT.with(|count| count.set(count.get() + 1));
        let screen = StyledScreen::parse(text).ok()?;
        match control::completion_draft(&screen) {
            control::CompletionDraft::Recognised { draft, selected } => {
                return Some(draft == expected && selected == expected);
            }
            control::CompletionDraft::Invalid => return None,
            control::CompletionDraft::Absent => {}
        }
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
    let mut lines: Vec<_> = text.lines().map(|s| s.trim_end_matches(' ')).collect();
    while lines.last().is_some_and(|s| s.is_empty()) {
        lines.pop();
    }
    result.facts = base_observe_screen(input, cursor);
    if let Some(trust) = trust_dialog(&lines) {
        result.dialog = trust;
        return result;
    }
    if result.facts.dialog == DialogState::Permission {
        result.dialog = ControlDialog::Blocked;
        return result;
    }
    match styled.map(completion_draft) {
        Some(CompletionDraft::Recognised { .. }) => {
            result.dialog = ControlDialog::None;
            return result;
        }
        Some(CompletionDraft::Invalid) => return result,
        _ => {}
    }
    let header = lines.iter().enumerate().find_map(|(i, line)| {
        line.starts_with(" ▐▛███▛█   Claude Code v")
            .then(|| lines.get(i..).and_then(header_end).map(|end| (i, i + end)))
            .flatten()
    });
    let Some((footer, before_footer)) = lines.split_last() else {
        return result;
    };
    let (footer, before_footer) = if footer.trim() == "Ctrl+Y to paste deleted text" {
        let Some(parts) = before_footer.split_last() else {
            return result;
        };
        parts
    } else {
        (footer, before_footer)
    };
    let footer = footer
        .trim()
        .split_once("  ")
        .map_or(footer.trim(), |(left, _)| left);
    if !mode_footer(footer) {
        return result;
    }
    // Without a complete header, only already-recognized ordinary facts apply.
    if let Some((start, end)) = header {
        if let Some((facts, _)) = editor_facts(&lines, styled, Some(end), before_footer, footer) {
            result.facts = facts;
        }
        if lines.get(..start).unwrap_or_default().iter().any(|line| {
            let trimmed = line.trim();
            line.starts_with("❯ ")
                || line.starts_with("❯\u{a0}")
                || line.starts_with("● ")
                || mode_footer(trimmed.split_once("  ").map_or(trimmed, |(left, _)| left))
        }) {
            result.facts.fresh_editor = FreshEditorEvidence::Unknown;
        }
    }
    if result.facts.dialog == DialogState::None
        && (header.is_some() || result.facts.turn != TurnState::Unknown)
    {
        result.dialog = ControlDialog::None;
        if let Some(lower) = before_footer.iter().rposition(|s| rule(s, '─')) {
            if let Some(top) = lines
                .get(..lower)
                .and_then(|s| s.iter().rposition(|s| rule(s, '─')))
            {
                let history = lines
                    .get(header.map_or(0, |(_, end)| end)..top)
                    .unwrap_or_default();
                result.conversation = history.iter().any(|s| {
                    s.trim()
                        .strip_prefix("❯ ")
                        .is_some_and(|s| !s.is_empty() && s != "/clear")
                }) && history.iter().any(|s| s.trim().starts_with("● "));
            }
        }
    }
    result
}

// Synthetic: cursor extremes/interior pin Plain refusal and Styled indifference.
const CURSORS: [Option<ScreenCursor>; 4] = [
    None,
    Some(ScreenCursor { column: 0, row: 0 }),
    Some(ScreenCursor { column: 2, row: 6 }),
    Some(ScreenCursor {
        column: u32::MAX,
        row: u32::MAX,
    }),
];

// Synthetic: expected strings exercise exact drafts and validation boundaries.
const EXPECTED: [&str; 17] = [
    CLEAR_COMMAND,
    "/other",
    " /clear",
    "baseline draft 123",
    "literal typed draft",
    "[m42] Read: atm inbox --id m42",
    "[m123] Read: atm inbox --id m123",
    "[m123456] Read: atm inbox --id m123456",
    "Try \"a sample task\"",
    "",
    "/clear ",
    "/clear\u{a0}",
    "/clear\u{2003}",
    "\n/clear",
    "\t",
    "\0",
    "\u{7f}",
];

fn captures() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut captures = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap();
        if name.starts_with("screen") && name.ends_with(".txt") {
            let text = std::fs::read_to_string(&path).unwrap();
            captures.push((name.to_owned(), body(&text).to_owned()));
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

fn compare_observers(name: &str, text: &str) {
    for (format, input) in [
        ("Plain", ScreenInput::Plain(text)),
        ("Styled", ScreenInput::Styled(text)),
    ] {
        for cursor in CURSORS {
            assert_eq!(
                observe_screen(input, cursor),
                base_observe_screen(input, cursor),
                "{name}: {format}, {cursor:?}"
            );
            assert_eq!(
                observe_control_screen(input, cursor),
                base_observe_control_screen(input, cursor),
                "{name}: {format}, {cursor:?}"
            );
        }
        for expected in EXPECTED {
            assert_eq!(
                staged_draft_matches(input, expected),
                base_staged_draft_matches(input, expected),
                "{name}: {format}, {expected:?}"
            );
        }
    }
}

#[test]
fn committed_capture_observers_match_base() {
    let captures = captures();
    for (name, text) in &captures {
        compare_observers(name, text);
    }
    println!(
        "EQUIVALENCE captures={} formats=2 cursors={} expected_strings={}",
        captures.len(),
        CURSORS.len(),
        EXPECTED.len()
    );
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
            for cursor in CURSORS {
                let (base, before) = counted(|| base_observe_control_screen(input, cursor));
                let (current, after) = counted(|| observe_control_screen(input, cursor));
                assert_eq!(current, base, "{name}: {format}, {cursor:?}");
                println!("{name}\t{format}\t{cursor:?}\t{before}\t{after}");
                *groups.entry((format, before, after)).or_insert(0) += 1;
                if after != expected {
                    violations.push((name.clone(), format, cursor, after));
                }
            }
        }
    }
    for ((format, before, after), looks) in groups {
        println!("PARSE_COUNTS {format}: {looks} looks, {before} -> {after} per look");
    }
    assert!(
        violations.is_empty(),
        "repeated decodes: {} violations; samples: {:?}",
        violations.len(),
        violations.iter().take(4).collect::<Vec<_>>()
    );
}

#[test]
fn synthetic_viewport_boundaries_match_base() {
    // Captured: Claude Code 2.1.325, 2026-11-14 through 2026-11-16,
    // private-tmux capture; individual methods are in fixtures/README.md.
    let fixtures = [
        include_str!("../../../tests/fixtures/screen_styled_u5b_poor-enabled-fresh_2.1.325.txt"),
        include_str!("../../../tests/fixtures/screen_styled_s1_sugg2-native-high-typed-home-control_2.1.325.txt"),
        include_str!("../../../tests/fixtures/screen_styled_s1_sugg2-native-low-suggestion_2.1.325.txt"),
        include_str!("../../../tests/fixtures/screen_styled_s1_sugg2-drawn-low-suggestion_2.1.325.txt"),
        include_str!("../../../tests/fixtures/screen_styled_s1_clear-multi-header_2.1.325.txt"),
        include_str!("../../../tests/fixtures/screen_control_p1_B-clear-001-120x40_2.1.325.txt"),
        include_str!("../../../tests/fixtures/screen_control_p1_A80-selected-80x40_2.1.325.txt"),
    ];
    for (case, fixture) in fixtures.into_iter().enumerate() {
        let text = body(fixture);
        // Synthetic: preserve the captured geometry except the named boundary.
        let mut variants = vec![
            ("ASCII tail", format!("{text}\n   \n")),
            ("NBSP tail", format!("{text}\n\u{a0}\n")),
            ("Unicode tail", format!("{text}\n\u{2003}\n")),
            ("leading blanks", format!("\n\n{text}")),
            (
                "copied prefix",
                format!("❯ copied prompt\n● copied reply\n{text}"),
            ),
        ];
        for (why, from, to) in [
            ("leading draft space", "❯\u{a0}", "❯\u{a0} "),
            ("selected command", "❯ /clear", "❯ /other"),
            ("faint removed", "\x1b[2m", "\x1b[0m"),
            ("inverse removed", "\x1b[7m", "\x1b[0m"),
            (
                "manual footer",
                "⏵⏵ auto mode on (shift+tab to cycle)",
                "⏸ manual mode on",
            ),
        ] {
            if text.contains(from) {
                variants.push((why, text.replace(from, to)));
            }
        }
        for (why, changed) in variants {
            assert_ne!(changed, text, "case {case}: {why}");
            compare_observers(&format!("case {case}: {why}"), &changed);
        }
    }
}

#[test]
fn malformed_viewport_refuses_every_entry_point() {
    // Captured: Claude Code 2.1.325, 2026-11-14 and 2026-11-15,
    // sealed private tmux; individual methods are in fixtures/README.md.
    let fixtures = [
        (
            "fresh",
            include_str!(
                "../../../tests/fixtures/screen_styled_u5b_poor-enabled-fresh_2.1.325.txt"
            ),
        ),
        (
            "draft",
            include_str!(
                "../../../tests/fixtures/screen_p3_120x40-r1-C-u-staged_2.1.325.styled.txt"
            ),
        ),
        (
            "above menu",
            include_str!("../../../tests/fixtures/screen_styled_s1_clear-single_2.1.325.txt"),
        ),
        (
            "below menu",
            include_str!(
                "../../../tests/fixtures/screen_control_p1_B-clear-001-120x40_2.1.325.txt"
            ),
        ),
    ];
    for (name, fixture) in fixtures {
        let text = body(fixture);
        if name.ends_with("menu") {
            assert_eq!(
                staged_draft_matches(ScreenInput::Styled(text), CLEAR_COMMAND),
                Some(true),
                "{name}"
            );
            assert_eq!(
                observe_control_screen(ScreenInput::Styled(text), None).dialog,
                ControlDialog::None,
                "{name}"
            );
        }
        // Synthetic: trim only ASCII allocation padding for a supported Plain twin.
        let plain = StyledScreen::parse(text)
            .unwrap()
            .text()
            .lines()
            .map(|row| row.trim_end_matches(' '))
            .collect::<Vec<_>>()
            .join("\n");
        for (format, text) in [("Styled", text), ("Plain", plain.as_str())] {
            let input = if format == "Styled" {
                ScreenInput::Styled(text)
            } else {
                ScreenInput::Plain(text)
            };
            if matches!(name, "fresh" | "draft") {
                assert_ne!(
                    observe_screen(input, None),
                    ScreenFacts::default(),
                    "{name}: {format}"
                );
                assert_ne!(
                    observe_control_screen(input, None),
                    ControlScreen::default(),
                    "{name}: {format}"
                );
            }
            // Synthetic: no supported observation survives invalid terminal data.
            for suffix in [
                "\x1b",
                "\x1b[",
                "\x1b[2",
                "\x1b[999m",
                "\x1b[4m",
                "\x1b[9m",
                "\x1b[1A",
                "\x1b]8;;https://example.invalid",
                "\x1b]0;title\x07",
                "\x1b[8m",
                "\r",
                "\t",
                "\0",
                "\x07",
                "\x7f",
                "\u{9b}",
            ] {
                let changed = format!("{text}{suffix}");
                let input = if format == "Styled" {
                    ScreenInput::Styled(&changed)
                } else {
                    ScreenInput::Plain(&changed)
                };
                for cursor in CURSORS {
                    assert_eq!(
                        observe_screen(input, cursor),
                        ScreenFacts::default(),
                        "{name}: {format}, {cursor:?}, {suffix:?}"
                    );
                    assert_eq!(
                        observe_control_screen(input, cursor),
                        ControlScreen::default(),
                        "{name}: {format}, {cursor:?}, {suffix:?}"
                    );
                }
                assert_eq!(
                    staged_draft_matches(input, CLEAR_COMMAND),
                    None,
                    "{name}: {format}, {suffix:?}"
                );
            }
        }
    }
}
