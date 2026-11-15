use atm_core::harness_control::{ControlDialog, ControlScreen};
use atm_core::{
    styled_screen::StyledScreen, DialogState, FreshEditorEvidence, InputState, ScreenInput,
    TurnState,
};
use atm_pi_adapter::screen::observe_control_screen;
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct Capture {
    source: String,
    fixture: String,
    width: usize,
    format: String,
    input: String,
    turn: String,
    dialog: String,
    fresh_editor: String,
    control_dialog: String,
    original_conversation: bool,
}

fn captures() -> Vec<Capture> {
    serde_json::from_str(include_str!("fixtures/control_startup/manifest.json"))
        .unwrap_or_else(|error| panic!("invalid capture manifest: {error}"))
}

fn text(capture: &Capture) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/control_startup")
            .join(&capture.fixture),
    )
    .unwrap_or_else(|error| panic!("cannot read {}: {error}", capture.fixture))
}

fn observe(text: &str) -> ControlScreen {
    observe_control_screen(ScreenInput::Styled(text), None)
}

// Captured: pi 1.5.0, 2026-11-14, P3/P5 sealed loopback-only private tmux
// capture-pane -p [-e] -N. Manifest maps every original to its sanitized body.
// Catches treating arbitrary startup suffixes as history, while freezing all
// independent facts (including Plain padding as Draft and popup Unknown).
#[test]
fn p3_p5_startup_frames_are_not_conversations() {
    let captures = captures();
    assert_eq!(captures.len(), 260);
    assert_eq!(
        captures.iter().filter(|c| c.original_conversation).count(),
        104
    );
    let mut positives = Vec::new();
    let mut widths = std::collections::BTreeSet::new();
    let mut empty = 0;
    let mut draft = 0;
    let mut unknown = 0;
    for capture in captures {
        widths.insert(capture.width);
        let text = text(&capture);
        let input = match capture.format.as_str() {
            "styled-N" => ScreenInput::Styled(&text),
            "plain-N" => ScreenInput::Plain(&text),
            other => panic!("unexpected format {other}"),
        };
        let result = observe_control_screen(input, None);
        let facts = &result.facts;
        for (got, want) in [
            (format!("{:?}", facts.input), capture.input),
            (format!("{:?}", facts.turn), capture.turn),
            (format!("{:?}", facts.dialog), capture.dialog),
            (format!("{:?}", facts.fresh_editor), capture.fresh_editor),
            (format!("{:?}", result.dialog), capture.control_dialog),
        ] {
            assert_eq!(got, want, "{}", capture.source);
        }
        empty += usize::from(facts.input == InputState::Empty);
        draft += usize::from(facts.input == InputState::Draft);
        unknown += usize::from(result.dialog == ControlDialog::Unknown);
        if result.conversation {
            positives.push(capture.source);
        }
    }
    assert_eq!(widths.into_iter().collect::<Vec<_>>(), [28, 80, 100, 120]);
    assert_eq!((empty, draft, unknown), (49, 157, 54));
    assert!(
        positives.is_empty(),
        "{} false positives: {positives:#?}",
        positives.len()
    );
}

fn wide_startup(width: usize) -> String {
    let capture = captures()
        .into_iter()
        .find(|c| {
            c.source == format!("cp-p5-captures/a-pi-cleanup/A-pi-{width}x40-empty.styled-N.txt")
        })
        .unwrap_or_else(|| panic!("missing {width}-column startup capture"));
    // Synthetic below: decode the real styled frame and remove only trailing
    // padding to make localized text mutations readable; no new capture claimed.
    StyledScreen::parse(&text(&capture))
        .unwrap_or_else(|error| panic!("invalid fixture styles: {error}"))
        .text()
        .lines()
        .map(|s| s.trim_end_matches(' '))
        .collect::<Vec<_>>()
        .join("\n")
}

fn suffix(base: &str, replacement: &str) -> String {
    let original = " fd not found. Downloading...\n Warning: Failed to download fd: fetch failed: getaddrinfo EAI_AGAIN github.com";
    assert!(base.contains(original));
    base.replace(original, replacement)
}

fn assert_editor_without_history(text: &str) {
    let result = observe(text);
    assert_eq!(result.facts.input, InputState::Empty, "{text}");
    assert_eq!(result.facts.turn, TurnState::NotRunning, "{text}");
    assert_eq!(result.facts.dialog, DialogState::None, "{text}");
    assert_eq!(result.dialog, ControlDialog::None, "{text}");
    assert!(!result.conversation, "{text}");
}

#[test]
fn startup_suffix_requires_complete_transcript_blocks() {
    // Synthetic: catches any-tail, count-only, any-valid-block and usage-only
    // shortcuts. Every block, not merely two qualifying ones, must be valid.
    for width in [100, 120] {
        let base = wide_startup(width);
        for tail in [
            " Checking optional resources...",
            " One visible prompt",
            " Checking resources...\n\n Warning: unavailable",
            " Warning: unavailable\n\n Checking resources...",
            " Checking resources...\n\n ✓ Notice",
            " ✓ Notice\n\n Checking resources...",
            " Checking resources...\n\n ↳ queued input",
            " ↳ queued input\n\n Checking resources...",
            "unindented\n\n Second block",
            " First block\n\n  double-indented",
            " First block\n\n Second block\n\n Warning: unavailable",
            " First block\n\n Second block\n\n ✓ Notice",
            " First block\n\n Second block\n\n ↳ queued input",
            " First block\n Warning: same block\n\n Second block",
        ] {
            assert_editor_without_history(&suffix(&base, tail));
        }
        for usage in ["0.0%/200k", "↑12 ↓3 R4 W5 $0.01 7.5%/500k"] {
            let fresh = suffix(&base, "").replace("0.0%/200k", usage);
            assert_editor_without_history(&fresh);
            let positive =
                suffix(&base, " A user prompt\n\n An assistant reply").replace("0.0%/200k", usage);
            let result = observe(&positive);
            assert_eq!(result.dialog, ControlDialog::None);
            assert_eq!(result.facts.input, InputState::Empty);
            assert_eq!(result.facts.fresh_editor, FreshEditorEvidence::Unknown);
            assert!(result.conversation, "{positive}");
        }
    }
}

#[test]
fn synthetic_startup_values_spacing_and_notices_are_not_history() {
    // Synthetic: real startup frames varied without adding any transcript.
    for width in [100, 120] {
        let base = wide_startup(width);
        for changed in [
            base.replace("v1.5.0", "v28.19.312"),
            base.replace("sealed-model", "other-model-v9"),
            base.replace("0.0%/200k", "↑12 ↓3 R4 W5 $0.01 7.5%/500k"),
            base.replacen("\n\n", "\n\n\n", 1),
            base.replace("\n Warning:", "\n\n Warning:"),
            suffix(&base, " fd not found. Downloading..."),
            suffix(&base, " Arbitrary startup status"),
            suffix(&base, " Warning: unavailable"),
            suffix(&base, " ✓ New session started"),
            base.replace("▀▀█", "???"),
            base.replace("Pi can explain", "Unknown startup"),
        ] {
            assert_ne!(changed, base);
            assert_editor_without_history(&changed);
        }
        // Move blank rows while preserving the logical startup anchors.
        let moved = base
            .replacen("\n\n", "\n", 1)
            .replacen("\n fd", "\n\n fd", 1);
        assert_ne!(moved, base);
        assert_editor_without_history(&moved);
    }
}

#[test]
fn synthetic_damaged_editor_footer_and_ansi_stay_unknown() {
    // Synthetic: evidence tightening must not broaden editor acceptance.
    for width in [100, 120] {
        let base = wide_startup(width);
        for damaged in [
            base.replace("0.0%/200k", "0.0%/"),
            base.replace("0.0%/200k", "unknown footer"),
            base.replace('─', "x"),
            base.replacen(&"─".repeat(width), &"─".repeat(width - 1), 1),
            format!("{base}\x1b[8m"),
            format!("{base}\x1b["),
        ] {
            assert_ne!(damaged, base);
            let result = observe(&damaged);
            assert_eq!(result.dialog, ControlDialog::Unknown);
            assert_eq!(result.facts, Default::default());
            assert!(!result.conversation);
        }
    }
}
