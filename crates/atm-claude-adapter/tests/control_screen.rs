use atm_claude_adapter::screen::{self, observe_control_screen, CONTROL_POLICY};
use atm_core::harness_control::{ClearProof, ControlDialog, ControlKey, SessionArrival};
use atm_core::styled_screen::StyledScreen;
use atm_core::{FreshEditorEvidence, InputState, ScreenInput, TurnState};

macro_rules! capture {
    ($name:literal) => {
        include_str!(concat!(
            "fixtures/screen_control_p1_",
            $name,
            "_2.1.325.txt"
        ))
    };
}
const NO: &str = capture!("A80-default-80x40");
const YES: &str = capture!("A80-selected-80x40");
const AUTO: &str = capture!("A-auto-ready-120x40");
fn observe(text: &str) -> atm_core::harness_control::ControlScreen {
    observe_control_screen(ScreenInput::Styled(body(text)), None)
}
fn body(text: &str) -> &str {
    if text.starts_with("Captured:") {
        text.split_once('\n').map_or(text, |(_, body)| body)
    } else {
        text
    }
}
fn plain(text: &str) -> String {
    StyledScreen::parse(body(text))
        .unwrap_or_else(|error| panic!("invalid fixture styles: {error}"))
        .text()
        .to_owned()
}

#[test]
fn captured_completion_verifies_editor_and_selected_command() {
    let menu = body(capture!("B-clear-001-120x40"));
    assert_eq!(
        screen::staged_draft_matches(ScreenInput::Styled(menu), "/clear"),
        Some(true)
    );
    assert_eq!(observe(menu).dialog, ControlDialog::None);
    assert_eq!(
        screen::observe_screen(ScreenInput::Styled(menu), None),
        Default::default()
    );
    // Synthetic: selected names compare exactly, descriptions are not command identity.
    let text = plain(menu);
    for changed in [
        text.replace("❯ /clear", "❯ /other"),
        text.replace("❯ /clear  ", "❯ /clear-x"),
        text.replace("❯\u{a0}/clear", "❯\u{a0} /clear"),
    ] {
        assert_eq!(
            screen::staged_draft_matches(ScreenInput::Styled(&changed), "/clear"),
            Some(false)
        );
    }
    let description = text.replace(
        "Start a new session with empty context; previous session stays on disk (resumable with",
        "A different description",
    );
    assert_eq!(
        screen::staged_draft_matches(ScreenInput::Styled(&description), "/clear"),
        Some(true)
    );
    // Synthetic: absent/doubled selection, missing editor, malformed layout and faint draft refuse.
    for changed in [
        text.replace("❯ /clear", "  /clear"),
        text.replace("  ❯ /clear", "  ❯ /clear   First\n  ❯ /other"),
        text.replace("❯\u{a0}/clear", ""),
        text.replace("❯\u{a0}/clear", "❯\u{a0}\x1b[2m/clear\x1b[22m"),
        format!("{menu}\nunknown footer"),
        format!("{menu}\x1b[8m"),
    ] {
        assert_eq!(
            screen::staged_draft_matches(ScreenInput::Styled(&changed), "/clear"),
            None
        );
        assert_eq!(observe(&changed).dialog, ControlDialog::Unknown);
    }
}

#[test]
fn policy_reuses_existing_constants() {
    assert_eq!(CONTROL_POLICY.capture, screen::SCREEN_TEXT_FORMAT);
    assert_eq!(CONTROL_POLICY.clear_command, screen::CLEAR_COMMAND);
    assert_eq!(CONTROL_POLICY.session_arrival, SessionArrival::AtStart);
    assert_eq!(
        CONTROL_POLICY.clear_proof,
        ClearProof::ChangedSession {
            start_source: Some("clear")
        }
    );
    assert_eq!(CONTROL_POLICY.trust_selection_retries, 1);
}

#[test]
fn real_trust_selection_and_first_frame_reset() {
    for (text, selected) in [
        (NO, false),
        (capture!("A120-default-120x40"), false),
        (capture!("A200-default-200x40"), false),
        (YES, true),
        (capture!("A120-selected-120x40"), true),
        (capture!("A200-selected-200x40"), true),
        (capture!("A80-reset-after-enter-80x40"), false),
        (capture!("A120-reset-after-enter-120x40"), false),
        (capture!("A200-reset-after-enter-200x40"), false),
        (capture!("A-race-0-yes-120x40"), true),
        (capture!("A-race-1-yes-120x40"), true),
    ] {
        let expected = ControlDialog::FolderTrust {
            folder: "/tmp/xxxxxxxxxxxxxxxxxxxxxxx/cwd".into(),
            selected_accept: selected,
            select_accept: (!selected).then_some(ControlKey::Down),
            accept: ControlKey::Enter,
        };
        assert_eq!(observe(text).dialog, expected);
        assert_eq!(
            observe_control_screen(ScreenInput::Plain(&plain(text)), None).dialog,
            expected
        );
    }
}

#[test]
fn captured_launch_and_clear_are_fresh_without_conversation() {
    for text in [
        AUTO,
        capture!("A80-ready-80x40"),
        capture!("A120-ready-120x40"),
        capture!("A200-ready-200x40"),
        capture!("B-clear-007-120x40"),
    ] {
        let observed = observe(text);
        assert_eq!(observed.dialog, ControlDialog::None);
        assert_eq!(observed.facts.input, InputState::Empty);
        assert_eq!(observed.facts.turn, TurnState::NotRunning);
        assert_eq!(observed.facts.fresh_editor, FreshEditorEvidence::Present);
        assert!(!observed.conversation);
    }
    // Synthetic: production U8e launch syntax, not the probe's shell command.
    let launched = AUTO.replace(
        "probe$ bash \"$RUN/launch-auto.sh\"",
        "shell$ cd '/tmp/project' && claude --model example",
    );
    assert_ne!(launched, AUTO);
    assert_eq!(observe(&launched), observe(AUTO));
    // Legacy facts remain unchanged.
    assert_eq!(
        screen::observe_screen(ScreenInput::Styled(body(AUTO)), None).turn,
        TurnState::Unknown
    );
    assert_eq!(
        screen::observe_screen(ScreenInput::Styled(body(YES)), None).dialog,
        atm_core::DialogState::Unknown
    );
}

#[test]
fn real_u5b_settled_clear_is_fresh_without_conversation() {
    let text =
        include_str!("fixtures/screen_styled_u5b_poor-enabled-after-clear-settled_2.1.325.txt");
    let observed = observe(text);
    assert_eq!(observed.dialog, ControlDialog::None);
    assert_eq!(observed.facts.input, InputState::Empty);
    assert_eq!(observed.facts.turn, TurnState::NotRunning);
    assert_eq!(observed.facts.fresh_editor, FreshEditorEvidence::Present);
    assert!(!observed.conversation);
}

#[test]
fn synthetic_live_suffix_on_every_captured_trust_selection_cannot_authorize_keys() {
    for text in [
        NO,
        capture!("A120-default-120x40"),
        capture!("A200-default-200x40"),
        YES,
        capture!("A120-selected-120x40"),
        capture!("A200-selected-200x40"),
        capture!("A80-reset-after-enter-80x40"),
        capture!("A120-reset-after-enter-120x40"),
        capture!("A200-reset-after-enter-200x40"),
        capture!("A-race-0-yes-120x40"),
        capture!("A-race-1-yes-120x40"),
    ] {
        assert!(matches!(
            observe(text).dialog,
            ControlDialog::FolderTrust { .. }
        ));
        // Synthetic: real actionable chrome is now history, with a current
        // fresh editor, conversation editor or shell below it.
        for suffix in [AUTO, capture!("B-baseline-conversation-120x40"), "shell$ "] {
            let mutated = format!("{}\n{}", body(text), body(suffix));
            let decoded = plain(&mutated);
            for input in [ScreenInput::Styled(&mutated), ScreenInput::Plain(&decoded)] {
                assert!(matches!(
                    observe_control_screen(input, None).dialog,
                    ControlDialog::None | ControlDialog::Unknown | ControlDialog::Blocked
                ));
            }
        }
    }
}

#[test]
fn conversation_is_not_a_draft_or_clear_echo() {
    for text in [
        capture!("B-baseline-conversation-120x40"),
        capture!("C1-auto-settled-120x40"),
        capture!("B-auto-submitted-120x40"),
    ] {
        assert!(observe(text).conversation);
        assert_eq!(observe(text).dialog, ControlDialog::None);
        assert_eq!(
            observe(text).facts.fresh_editor,
            FreshEditorEvidence::Unknown
        );
    }
    // Synthetic: typed composer and lone slash echo are not a transcript.
    let draft = plain(AUTO).replace("❯\u{a0}", "❯\u{a0}draft");
    assert!(!observe(&draft).conversation);
    let old = plain(capture!("B-clear-007-120x40"))
        .replace("❯ /clear", "❯ previous prompt\n● previous reply\n❯ /clear");
    assert!(observe(&old).conversation);
    assert_eq!(
        observe(&old).facts.fresh_editor,
        FreshEditorEvidence::Unknown
    );
}

#[test]
fn synthetic_damaged_trust_or_active_suffix_never_authorizes_keys() {
    let no = plain(NO);
    for text in [
        no.replace("Quick safety check:", "Safety?"),
        no.replace("Security guide", "Guide"),
        no.replace("Enter to confirm", "Enter to delete"),
        no.replace("❯ No, exit", "No, exit"),
        no.replace("Yes, I trust", "❯ Yes, I trust"),
        no.replace("/cwd", "/..."),
        no.replace("/cwd", "/long\n wrapped"),
        no.replace("No, exit", "No, continue"),
        format!("{NO}\x1b[8m"),
        format!("{NO}\x1b["),
        format!("{NO}\nunknown footer"),
    ] {
        assert_eq!(observe(&text).dialog, ControlDialog::Unknown, "{text}");
    }
    for text in [NO, YES] {
        // Synthetic: historical dialog above a current editor or a shell.
        assert_eq!(
            observe(&format!("{text}\n{AUTO}")).dialog,
            ControlDialog::None
        );
        assert_eq!(
            observe(&format!("{text}\nshell$ ")).dialog,
            ControlDialog::Unknown
        );
    }
}

#[test]
fn synthetic_scrolled_transcript_before_a_complete_header_is_not_fresh() {
    // Synthetic: the original header scrolled away, leaving real transcript rows
    // above a copied complete header and the captured fresh editor/footer.
    let baseline = plain(capture!("B-baseline-conversation-120x40"));
    let transcript = baseline
        .split_once("❯ /clear")
        .unwrap()
        .1
        .split_once('─')
        .unwrap()
        .0;
    let auto = plain(AUTO);
    let fresh = &auto[auto.find(" ▐▛███▛█   Claude Code v").unwrap()..];
    let editor = auto.lines().find(|row| row.starts_with("❯\u{a0}")).unwrap();
    let rule = auto.lines().find(|row| row.starts_with('─')).unwrap();
    for prefix in [
        format!("❯ /clear{transcript}"),
        "❯ old prompt".into(),
        "● old reply".into(),
        format!("{rule}\n{editor}\n{rule}"),
        "  ⏵⏵ auto mode on (shift+tab to cycle)".into(),
        "  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents".into(),
        "  ⏸ manual mode on".into(),
        "  ⏸ manual mode on · ← for agents  right-aligned notice".into(),
    ] {
        let text = format!("{prefix}\n{fresh}");
        for input in [ScreenInput::Styled(&text), ScreenInput::Plain(&text)] {
            let observed = observe_control_screen(input, None);
            assert_eq!(observed.dialog, ControlDialog::None, "{prefix}");
            assert_eq!(
                observed.facts.fresh_editor,
                FreshEditorEvidence::Unknown,
                "{prefix}"
            );
        }
    }
    // Synthetic: an ordinary shell preamble remains uninterpreted.
    assert_eq!(
        observe(&format!("shell$ cd '/tmp/project' && claude\n{fresh}"))
            .facts
            .fresh_editor,
        FreshEditorEvidence::Present
    );
}

#[test]
fn synthetic_column_zero_shell_prompt_before_header_vetoes_freshness() {
    // Synthetic: the B0 tail review's shell launch line above A-auto-ready's
    // real header. A shell prompt indistinguishable from transcript chrome
    // conservatively vetoes freshness; it does not block the ready editor.
    let auto = plain(AUTO);
    let fresh = &auto[auto.find(" ▐▛███▛█   Claude Code v").unwrap()..];
    let text = format!("❯ cd '/tmp/project' && claude --model example\n{fresh}");
    for input in [ScreenInput::Plain(&text), ScreenInput::Styled(&text)] {
        let observed = observe_control_screen(input, None);
        assert_eq!(observed.dialog, ControlDialog::None);
        assert_eq!(observed.facts.fresh_editor, FreshEditorEvidence::Unknown);
    }
}

#[test]
fn synthetic_headers_and_footer_damage_do_not_manufacture_freshness() {
    let auto = plain(AUTO);
    for text in [
        auto.replace("Claude Code v", "Not a header v"),
        auto.replace("cycle)", "changed)"),
        format!("{AUTO}\nunknown footer"),
        format!("{AUTO}\x1b[8m"),
    ] {
        assert_eq!(observe(&text).dialog, ControlDialog::Unknown);
        assert_eq!(
            observe(&text).facts.fresh_editor,
            FreshEditorEvidence::Unknown
        );
    }
    // Synthetic: first complete header is the boundary; a second copy is history, not a restart.
    let copied = format!("{auto}\n{auto}");
    assert_eq!(
        observe(&copied).facts.fresh_editor,
        FreshEditorEvidence::Unknown
    );
    assert_eq!(
        observe(capture!("C3-dialog-120x40")).dialog,
        ControlDialog::Unknown
    );
}
