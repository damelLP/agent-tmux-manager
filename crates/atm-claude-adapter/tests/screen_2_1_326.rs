//! Captured: Claude Code 2.1.326, 2026-11-17, sealed fake-API TUI.
//!
//! Stock CLI, tmux 3.8, fullscreen 116x40, private HOME/config/socket/PID view,
//! loopback-only network, generated synthetic credentials and canned Messages SSE.
//! Both connector opt-outs, strict empty MCP and the supervised guard/watcher
//! remained enabled. No owner setup, provider request or paid call was used.
//! Native cursor flags were 1; drawn flags were 0. Coordinates are not parser inputs.
//! No permission/trust dialog was accepted; selected trust means Down only.
//!
//! Each stem has real `capture-pane -p -e -N` (styled) and `-p -N` (plain)
//! siblings. Sequential captures are not atomic twins. Only private root prefixes
//! become equal-width generic paths; SGR, rows, glyphs and allocated padding stay.
//! `startup-animated` preserves earlier real frames without the canonical avatar;
//! the primary fresh frames additionally waited for the canonical three-row header.
//! Other stems name their actual state. A fresh/draft frame after clear can still
//! have unsupported deleted-text/auto-mode prose; it is not silently cleaned up.
//!
//! These pin the supported reader and its remaining gaps. The observed busy auto
//! footer includes `esc to interrupt` and preserves Running, never idle evidence.
//! Hook-generated permission prose is still outside the captured ask-rule grammar
//! and refuses rather than authorizing keys. Neither these observations nor a
//! visible fresh layout prove a daemon workflow or clear session identity.

use atm_claude_adapter::screen;
use atm_core::harness_control::{ControlDialog, ControlKey};
use atm_core::{DialogState, FreshEditorEvidence, InputState, ScreenFacts, ScreenInput, TurnState};

const DRAFT: &str = "literal typed draft";
const SUGGESTION: &str = "check the neutral fixture after this completed turn";
const FOLDER: &str = "/tmp/xxxxxxxxxxxxxxxxxxxxxxxxxxxxx/trust/project";

macro_rules! capture {
    ($stem:expr, $format:literal) => {
        include_str!(concat!(
            "fixtures/screen_cc326-",
            $stem,
            "_2.1.326.",
            $format,
            ".txt"
        ))
    };
}

fn body(captured: &str) -> &str {
    captured.split_once('\n').map_or(captured, |(_, text)| text)
}

fn facts(input: InputState, turn: TurnState, fresh: FreshEditorEvidence) -> ScreenFacts {
    let mut expected = ScreenFacts::default();
    expected.input = input;
    expected.turn = turn;
    expected.dialog = DialogState::None;
    expected.fresh_editor = fresh;
    expected
}

fn editor_case(
    styled: &str,
    plain: &str,
    expected: ScreenFacts,
    dialog: ControlDialog,
    conversation: bool,
    staged: Option<&str>,
) {
    let text = body(styled);
    let input = ScreenInput::Styled(text);
    assert_eq!(screen::observe_screen(input, None), expected);
    let control = screen::observe_control_screen(input, None);
    assert_eq!(control.facts, expected);
    assert_eq!(control.dialog, dialog);
    assert_eq!(control.conversation, conversation);
    for query in [DRAFT, SUGGESTION, "/clear"] {
        assert_eq!(
            screen::staged_draft_matches(input, query),
            staged.map(|draft| draft == query),
            "{query}"
        );
        // Plain retained-grid text cannot prove the styled exact-stage contract.
        assert_eq!(
            screen::staged_draft_matches(ScreenInput::Plain(body(plain)), query),
            None
        );
    }
    // Synthetic: append unsupported conceal; no plain fallback may authorize input.
    let malformed = format!("{text}\x1b[8m");
    let malformed = ScreenInput::Styled(&malformed);
    assert_eq!(screen::observe_screen(malformed, None), Default::default());
    assert_eq!(
        screen::observe_control_screen(malformed, None),
        Default::default()
    );
    assert_eq!(screen::staged_draft_matches(malformed, "/clear"), None);
}

#[test]
fn captured_fresh_editor_is_empty_and_idle() {
    for (styled, plain) in [
        (
            capture!("native-fresh", "styled"),
            capture!("native-fresh", "plain"),
        ),
        (
            capture!("drawn-fresh", "styled"),
            capture!("drawn-fresh", "plain"),
        ),
    ] {
        editor_case(
            styled,
            plain,
            facts(
                InputState::Empty,
                TurnState::NotRunning,
                FreshEditorEvidence::Present,
            ),
            ControlDialog::None,
            false,
            None,
        );
    }
}

#[test]
fn captured_finished_turns_and_suggestions_are_idle_but_never_fresh_or_staged() {
    macro_rules! mode {
        ($mode:literal) => {
            for (styled, plain) in [
                (
                    capture!(concat!($mode, "-finished-first-turn"), "styled"),
                    capture!(concat!($mode, "-finished-first-turn"), "plain"),
                ),
                (
                    capture!(concat!($mode, "-finished"), "styled"),
                    capture!(concat!($mode, "-finished"), "plain"),
                ),
                (
                    capture!(concat!($mode, "-suggestion"), "styled"),
                    capture!(concat!($mode, "-suggestion"), "plain"),
                ),
            ] {
                editor_case(
                    styled,
                    plain,
                    facts(
                        InputState::Empty,
                        TurnState::NotRunning,
                        FreshEditorEvidence::Unknown,
                    ),
                    ControlDialog::None,
                    true,
                    None,
                );
            }
        };
    }
    mode!("native");
    mode!("drawn");
}

#[test]
fn captured_same_suggestion_text_really_typed_is_a_draft_even_at_home() {
    macro_rules! mode {
        ($mode:literal) => {
            for (styled, plain) in [
                (
                    capture!(concat!($mode, "-same-text-draft"), "styled"),
                    capture!(concat!($mode, "-same-text-draft"), "plain"),
                ),
                (
                    capture!(concat!($mode, "-same-text-draft-home"), "styled"),
                    capture!(concat!($mode, "-same-text-draft-home"), "plain"),
                ),
            ] {
                editor_case(
                    styled,
                    plain,
                    facts(
                        InputState::Draft,
                        TurnState::NotRunning,
                        FreshEditorEvidence::Unknown,
                    ),
                    ControlDialog::None,
                    true,
                    Some(SUGGESTION),
                );
            }
        };
    }
    mode!("native");
    mode!("drawn");
}

#[test]
fn captured_post_clear_notices_cannot_prove_idle_or_freshness() {
    macro_rules! mode {
        ($mode:literal) => {
            editor_case(
                capture!(concat!($mode, "-after-clear"), "styled"),
                capture!(concat!($mode, "-after-clear"), "plain"),
                facts(
                    InputState::Empty,
                    TurnState::Unknown,
                    FreshEditorEvidence::Unknown,
                ),
                ControlDialog::None,
                false,
                None,
            );
            for (styled, plain) in [
                (
                    capture!(concat!($mode, "-draft"), "styled"),
                    capture!(concat!($mode, "-draft"), "plain"),
                ),
                (
                    capture!(concat!($mode, "-draft-home"), "styled"),
                    capture!(concat!($mode, "-draft-home"), "plain"),
                ),
            ] {
                editor_case(
                    styled,
                    plain,
                    facts(
                        InputState::Draft,
                        TurnState::Unknown,
                        FreshEditorEvidence::Unknown,
                    ),
                    ControlDialog::None,
                    false,
                    Some(DRAFT),
                );
            }
        };
    }
    mode!("native");
    mode!("drawn");
}

#[test]
fn captured_clear_completion_matches_editor_and_selection_not_readiness() {
    for (styled, plain) in [
        (
            capture!("native-clear-completion", "styled"),
            capture!("native-clear-completion", "plain"),
        ),
        (
            capture!("drawn-clear-completion", "styled"),
            capture!("drawn-clear-completion", "plain"),
        ),
    ] {
        editor_case(
            styled,
            plain,
            facts(
                InputState::Draft,
                TurnState::Unknown,
                FreshEditorEvidence::Unknown,
            ),
            ControlDialog::None,
            false,
            Some("/clear"),
        );
        // Synthetic: strip SGR, then change only the selected completion, not the editor.
        let decoded = atm_core::styled_screen::StyledScreen::parse(body(styled)).unwrap();
        let other = decoded.text().replace("  ❯ /clear", "  ❯ /other");
        assert_ne!(other, decoded.text());
        assert_eq!(
            screen::staged_draft_matches(ScreenInput::Styled(&other), "/clear"),
            Some(false)
        );
    }
}

fn busy_case(styled: &str, plain: &str, plain_control_input: InputState) {
    let expected = facts(
        InputState::Empty,
        TurnState::Running,
        FreshEditorEvidence::Unknown,
    );
    editor_case(
        styled,
        plain,
        expected.clone(),
        ControlDialog::None,
        false,
        None,
    );
    // Plain -N retains allocated editor padding, which is not proof of Empty.
    let plain_expected = facts(
        InputState::Draft,
        TurnState::Running,
        FreshEditorEvidence::Unknown,
    );
    assert_eq!(
        screen::observe_screen(ScreenInput::Plain(body(plain)), None),
        plain_expected
    );
    // A recognized canonical header re-reads trimmed rows in the control path;
    // the native animated header does not. Preserve that pre-existing difference.
    let control_expected = facts(
        plain_control_input,
        TurnState::Running,
        FreshEditorEvidence::Unknown,
    );
    let plain_control = screen::observe_control_screen(ScreenInput::Plain(body(plain)), None);
    assert_eq!(plain_control.facts, control_expected);
    assert_eq!(plain_control.dialog, ControlDialog::None);
    // Synthetic: removing only the segment preserves that pre-existing plain-input contract.
    let without_busy = body(plain).replace(" · esc to interrupt", "");
    assert_ne!(without_busy, body(plain));
    assert_eq!(
        screen::observe_screen(ScreenInput::Plain(&without_busy), None),
        plain_expected
    );
    assert_eq!(
        screen::observe_control_screen(ScreenInput::Plain(&without_busy), None).facts,
        control_expected
    );
    // Synthetic: the existing optional agents hint is absent; only the busy segment remains.
    let without_agents = body(styled).replace(" · ← for agents", "");
    assert_ne!(without_agents, body(styled));
    let control = screen::observe_control_screen(ScreenInput::Styled(&without_agents), None);
    assert_eq!(control.facts, expected);
    assert_eq!(control.dialog, ControlDialog::None);
}

#[test]
fn captured_native_busy_footer_preserves_running() {
    busy_case(
        capture!("native-busy", "styled"),
        capture!("native-busy", "plain"),
        InputState::Draft,
    );
}

#[test]
fn captured_drawn_busy_footer_preserves_running() {
    busy_case(
        capture!("drawn-busy", "styled"),
        capture!("drawn-busy", "plain"),
        InputState::Empty,
    );
}

fn with_busy_footer(captured: &str, agents: bool) -> String {
    let original = body(captured);
    let busy = original.replace(" · ← for agents", " · esc to interrupt · ← for agents");
    assert_ne!(busy, original, "busy-footer mutation must change the frame");
    if agents {
        busy
    } else {
        busy.replace(" · ← for agents", "")
    }
}

fn assert_busy_unknown(text: &str) {
    let input = ScreenInput::Styled(text);
    assert_eq!(screen::observe_screen(input, None), Default::default());
    assert_eq!(
        screen::observe_control_screen(input, None),
        Default::default()
    );
    assert_eq!(screen::staged_draft_matches(input, "/clear"), None);
}

#[test]
fn synthetic_busy_footer_cannot_make_completed_turn_ready() {
    // Synthetic: only transplant the captured busy segment onto each real
    // completed-turn frame; contradictory chrome must not authorize input.
    for source in [
        capture!("native-finished", "styled"),
        capture!("drawn-finished", "styled"),
    ] {
        for agents in [true, false] {
            assert_busy_unknown(&with_busy_footer(source, agents));
        }
    }
}

#[test]
fn synthetic_busy_footer_cannot_make_fresh_header_ready() {
    // Synthetic: the canonical fresh header is real, but its busy footer is
    // transplanted; the combination cannot prove idle or fresh readiness.
    for source in [
        capture!("native-fresh", "styled"),
        capture!("drawn-fresh", "styled"),
    ] {
        for agents in [true, false] {
            assert_busy_unknown(&with_busy_footer(source, agents));
        }
    }
}

#[test]
fn synthetic_manual_busy_footer_remains_unknown() {
    // Synthetic: manual busy mode has no capture; neither footer variant may
    // become supported by recognizing the captured auto-mode busy segment.
    for source in [
        capture!("native-busy", "styled"),
        capture!("drawn-busy", "styled"),
    ] {
        let original = atm_core::styled_screen::StyledScreen::parse(body(source))
            .unwrap()
            .text()
            .to_owned();
        let manual = original.replace("⏵⏵ auto mode on (shift+tab to cycle)", "⏸ manual mode on");
        assert_ne!(manual, original);
        for text in [manual.clone(), manual.replace(" · ← for agents", "")] {
            assert_busy_unknown(&text);
        }
    }
}

#[test]
fn synthetic_partial_other_and_misaligned_busy_segments_stay_unknown() {
    use atm_core::styled_screen::StyledScreen;
    for captured in [
        capture!("native-busy", "styled"),
        capture!("drawn-busy", "styled"),
    ] {
        let text = StyledScreen::parse(body(captured))
            .unwrap()
            .text()
            .to_owned();
        // Synthetic: change only the observed inline busy-footer segment.
        for segment in [
            " · esc to interrup",
            " · escape to interrupt",
            " · esc to interrupt now",
            " · other prose",
            " ·esc to interrupt",
            " · esc  to interrupt",
            " · esc to  interrupt",
            "\n · esc to interrupt",
            " · esc to interrupt · esc to interrupt",
        ] {
            let changed = text.replace(" · esc to interrupt", segment);
            assert_ne!(changed, text, "{segment}");
            let input = ScreenInput::Styled(&changed);
            assert_eq!(
                screen::observe_screen(input, None),
                Default::default(),
                "{segment}"
            );
            assert_eq!(
                screen::observe_control_screen(input, None),
                Default::default(),
                "{segment}"
            );
        }
    }
}

#[test]
fn captured_hook_permission_remains_unknown() {
    for input in [
        ScreenInput::Styled(body(capture!("permission-dialog", "styled"))),
        ScreenInput::Plain(body(capture!("permission-dialog", "plain"))),
    ] {
        assert_eq!(screen::observe_screen(input, None), Default::default());
        assert_eq!(
            screen::observe_control_screen(input, None),
            Default::default()
        );
        assert_eq!(screen::staged_draft_matches(input, "/clear"), None);
    }
}

#[test]
fn captured_animated_startup_is_not_a_canonical_fresh_header() {
    for (styled, plain) in [
        (
            capture!("native-startup-animated", "styled"),
            capture!("native-startup-animated", "plain"),
        ),
        (
            capture!("drawn-startup-animated", "styled"),
            capture!("drawn-startup-animated", "plain"),
        ),
    ] {
        editor_case(
            styled,
            plain,
            facts(
                InputState::Empty,
                TurnState::Unknown,
                FreshEditorEvidence::Unknown,
            ),
            ControlDialog::Unknown,
            false,
            None,
        );
    }
}

#[test]
fn captured_trust_selection_matches_the_old_control_contract_without_accepting() {
    for (selected, styled, plain) in [
        (
            false,
            capture!("trust-default", "styled"),
            capture!("trust-default", "plain"),
        ),
        (
            true,
            capture!("trust-selected", "styled"),
            capture!("trust-selected", "plain"),
        ),
    ] {
        for input in [
            ScreenInput::Styled(body(styled)),
            ScreenInput::Plain(body(plain)),
        ] {
            let mut expected = ScreenFacts::default();
            if !selected {
                expected.dialog = DialogState::FolderTrust {
                    folder: FOLDER.into(),
                };
            }
            assert_eq!(screen::observe_screen(input, None), expected);
            let control = screen::observe_control_screen(input, None);
            assert_eq!(control.facts, expected);
            assert_eq!(
                control.dialog,
                ControlDialog::FolderTrust {
                    folder: FOLDER.into(),
                    selected_accept: selected,
                    select_accept: (!selected).then_some(ControlKey::Down),
                    accept: ControlKey::Enter,
                }
            );
            assert!(!control.conversation);
            assert_eq!(screen::staged_draft_matches(input, "/clear"), None);
        }
    }
}

#[test]
fn synthetic_unfaint_suggestion_never_hides_authored_text() {
    use atm_core::styled_screen::StyledScreen;
    for captured in [
        capture!("native-suggestion", "styled"),
        capture!("drawn-suggestion", "styled"),
    ] {
        // Synthetic: remove all attributes from the real ghost, but feed Styled.
        let plain = StyledScreen::parse(body(captured))
            .unwrap()
            .text()
            .to_owned();
        let input = ScreenInput::Styled(&plain);
        assert_eq!(screen::observe_screen(input, None).input, InputState::Draft);
        assert_eq!(screen::staged_draft_matches(input, SUGGESTION), Some(true));
        assert_eq!(screen::staged_draft_matches(input, "/clear"), Some(false));
    }
}
