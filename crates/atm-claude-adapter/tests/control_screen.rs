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
fn captured_clear_context_hint_preserves_control_and_conversation() {
    for fixture in [
        include_str!(
            "fixtures/screen_styled_s1_sugg2-native-high-hint-only-after-three-turns_2.1.325.txt"
        ),
        include_str!(
            "fixtures/screen_styled_s1_sugg2-drawn-high-hint-only-after-three-turns_2.1.325.txt"
        ),
        include_str!("fixtures/screen_styled_s1_sugg2-native-high-hint-only-80_2.1.325.txt"),
        include_str!("fixtures/screen_styled_s1_sugg2-drawn-high-hint-only-160_2.1.325.txt"),
        include_str!("fixtures/screen_styled_s1_sugg2-native-high-hint-and-suggestion_2.1.325.txt"),
        include_str!("fixtures/screen_styled_s1_sugg2-drawn-high-hint-and-suggestion_2.1.325.txt"),
    ] {
        let control = observe(fixture);
        assert_eq!(control.dialog, ControlDialog::None);
        assert_eq!(control.facts.input, InputState::Empty);
        assert_eq!(control.facts.turn, TurnState::NotRunning);
        assert_eq!(control.facts.dialog, atm_core::DialogState::None);
        assert_eq!(control.facts.fresh_editor, FreshEditorEvidence::Unknown);
        assert!(control.conversation);
    }
    for fixture in [
        include_str!("fixtures/screen_styled_s1_sugg2-native-high-typed-home-control_2.1.325.txt"),
        include_str!("fixtures/screen_styled_s1_sugg2-drawn-high-typed-home-control_2.1.325.txt"),
    ] {
        let control = observe(fixture);
        assert_eq!(control.dialog, ControlDialog::None);
        assert_eq!(control.facts.input, InputState::Draft);
        assert_eq!(control.facts.turn, TurnState::NotRunning);
        assert!(control.conversation);
    }
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
fn custom_status_control_preserves_editor() {
    for (fresh, draft) in [
        (
            include_str!("fixtures/screen_styled_s1_status-80-fresh_2.1.325.txt"),
            include_str!("fixtures/screen_styled_s1_status-80-draft_2.1.325.txt"),
        ),
        (
            include_str!("fixtures/screen_styled_s1_status-116-fresh_2.1.325.txt"),
            include_str!("fixtures/screen_styled_s1_status-116-draft_2.1.325.txt"),
        ),
        (
            include_str!("fixtures/screen_styled_s1_status-160-fresh_2.1.325.txt"),
            include_str!("fixtures/screen_styled_s1_status-160-draft_2.1.325.txt"),
        ),
    ] {
        for (fixture, input, fresh_editor) in [
            (fresh, InputState::Empty, FreshEditorEvidence::Present),
            (draft, InputState::Draft, FreshEditorEvidence::Unknown),
        ] {
            let control = observe(fixture);
            assert_eq!(control.dialog, ControlDialog::None);
            assert_eq!(control.facts.input, input);
            // The real draft has a late, unsupported auto-mode disclaimer.
            assert_eq!(
                control.facts.turn,
                if input == InputState::Empty {
                    TurnState::NotRunning
                } else {
                    TurnState::Unknown
                }
            );
            assert_eq!(control.facts.fresh_editor, fresh_editor);
            assert!(!control.conversation);
        }
    }
}

#[test]
fn synthetic_menu_like_status_preserves_manual_and_auto_editors() {
    for (fixture, conversation, fresh) in [
        (
            capture!("B-baseline-conversation-120x40"),
            true,
            FreshEditorEvidence::Unknown,
        ),
        (
            capture!("B-clear-007-120x40"),
            false,
            FreshEditorEvidence::Present,
        ),
        (AUTO, false, FreshEditorEvidence::Present),
    ] {
        let text = body(fixture);
        let footer = text
            .lines()
            .find(|row| row.contains('⏸') || row.contains("⏵⏵"))
            .unwrap();
        // Synthetic: opaque status values inserted into real manual/auto frames.
        for status in [
            "  ❯ main",
            "  /srv/proj  main",
            "  Opus 5.5 /srv/proj  main",
            "  [plain] ctx 7%",
        ] {
            let changed = text.replace(footer, &format!("{status}\n{footer}"));
            assert_ne!(changed, text);
            let control = observe(&changed);
            assert_eq!(control.dialog, ControlDialog::None, "{status}");
            assert_eq!(control.facts.input, InputState::Empty, "{status}");
            assert_eq!(control.facts.turn, TurnState::NotRunning, "{status}");
            assert_eq!(control.facts.dialog, atm_core::DialogState::None);
            assert_eq!(control.facts.fresh_editor, fresh, "{status}");
            assert_eq!(control.conversation, conversation, "{status}");
            assert_eq!(
                screen::staged_draft_matches(ScreenInput::Styled(&changed), "/clear"),
                None
            );
        }
    }
}

#[test]
fn custom_status_clear_menu_above_editor() {
    for fixture in [
        include_str!("fixtures/screen_styled_s1_status-clear-80_2.1.325.txt"),
        include_str!("fixtures/screen_styled_s1_status-clear-116_2.1.325.txt"),
    ] {
        let original = body(fixture);
        // Synthetic: remove only the known fixture's status row, retaining styles.
        let without_status = original
            .lines()
            .filter(|line| !line.contains("[slash]"))
            .collect::<Vec<_>>()
            .join("\n");
        for text in [original, without_status.as_str()] {
            let input = ScreenInput::Styled(text);
            assert_eq!(screen::staged_draft_matches(input, "/clear"), Some(true));
            assert_eq!(
                screen::staged_draft_matches(input, "/clear-other"),
                Some(false)
            );
            assert_eq!(screen::staged_draft_matches(input, " /clear"), Some(false));
            let control = observe(text);
            assert_eq!(control.dialog, ControlDialog::None);
            assert_eq!(control.facts.input, InputState::Draft);
            // Menu history is not evidence of an idle turn; exact staging is separate.
            assert_eq!(control.facts.turn, TurnState::Unknown);
            assert_eq!(control.facts.fresh_editor, FreshEditorEvidence::Unknown);
        }
        assert_eq!(observe(original), observe(&without_status));
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
        "  ⏵⏵ auto mode on · 2 shells".into(),
        "  ⏸ manual mode on · 2 shells".into(),
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
fn synthetic_column_zero_service_status_before_header_vetoes_freshness() {
    // Synthetic: no captured shell preamble has this shape. A systemctl-style
    // status line above A-auto-ready's real header resembles transcript chrome
    // and conservatively vetoes freshness without blocking the ready editor.
    let auto = plain(AUTO);
    let fresh = &auto[auto.find(" ▐▛███▛█   Claude Code v").unwrap()..];
    let text = format!("● example.service - Example service\n{fresh}");
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

// Captured: Claude Code 2.1.325, 2026-11-15, sealed tmux capture-pane -p -e -N, no prompt.
// README maps the actual -p -e -N sources and equal-width private-cwd sanitization.
const ABOVE_COMPLETIONS: [&str; 6] = [
    include_str!("fixtures/screen_styled_s1_clear-multi-wrapped_2.1.325.txt"),
    include_str!("fixtures/screen_styled_s1_clear-single_2.1.325.txt"),
    include_str!("fixtures/screen_styled_s1_clear-multi-80_2.1.325.txt"),
    include_str!("fixtures/screen_styled_s1_clear-multi-160_2.1.325.txt"),
    include_str!("fixtures/screen_styled_s1_clear-multi-header_2.1.325.txt"),
    include_str!("fixtures/screen_styled_s1_clear-multi-drawn_2.1.325.txt"),
];
const NEUTRAL_HEADER: &str = " ▐▛███▛█   Claude Code v2.1.325\n▝▜██████▀  Demo Model · API Usage Billing\n ▝▝   ▝▝   /tmp/fixture\n\n";

#[test]
fn captured_above_completion_with_and_without_header() {
    // Removing above-menu recognition must fail on the real no-header frames.
    for (case, fixture) in ABOVE_COMPLETIONS.into_iter().enumerate() {
        let input = ScreenInput::Styled(body(fixture));
        for cursor in [None, Some(atm_core::ScreenCursor { column: 0, row: 0 })] {
            let control = observe_control_screen(input, cursor);
            assert_eq!(control.dialog, ControlDialog::None, "case {case}");
            assert_eq!(
                (
                    control.facts.input,
                    control.facts.turn,
                    control.facts.dialog,
                    control.facts.fresh_editor
                ),
                (
                    InputState::Draft,
                    TurnState::Unknown,
                    atm_core::DialogState::None,
                    FreshEditorEvidence::Unknown
                ),
                "case {case}"
            );
        }
        assert_eq!(
            screen::staged_draft_matches(input, "/clear"),
            Some(true),
            "case {case}"
        );
        for wrong in ["/clear-other", " /clear", "/clea"] {
            assert_eq!(
                screen::staged_draft_matches(input, wrong),
                Some(false),
                "case {case}: {wrong}"
            );
        }
        assert_eq!(
            screen::staged_draft_matches(ScreenInput::Plain(&plain(fixture)), "/clear"),
            None
        );
    }
}

#[test]
fn synthetic_top_cut_completion_continuations_refuse_staging() {
    let text = plain(ABOVE_COMPLETIONS.first().unwrap());
    let rule = text.lines().find(|row| row.starts_with('─')).unwrap();
    let (_, editor) = text.split_once(rule).unwrap();
    // Synthetic: the pane top cuts off every command, leaving only a real
    // description continuation above the editor, with optional blank padding.
    let continuations: Vec<_> = text
        .lines()
        .filter(|row| row.contains("on disk") || row.contains("consistent wording"))
        .collect();
    assert_eq!(continuations.len(), 2);
    for continuation in continuations {
        for padding in ["", "\n\n"] {
            let cut = format!("{padding}{continuation}\n{rule}{editor}");
            assert_eq!(
                screen::staged_draft_matches(ScreenInput::Styled(&cut), "/clear"),
                None,
                "{cut}"
            );
            assert_eq!(observe(&cut).dialog, ControlDialog::Unknown);
        }
    }
}

#[test]
fn above_completion_checks_selection_before_fallback() {
    // Ignoring the selected name, or falling back after a bad menu, must fail.
    let text = plain(ABOVE_COMPLETIONS.first().unwrap());
    // Synthetic: one name differs, retaining the real frame and description columns.
    for changed in [
        text.replace("  ❯ /clear", "  ❯ /other"),
        text.replace("❯\u{a0}/clear", "❯\u{a0}/other"),
    ] {
        for text in [changed.clone(), format!("{NEUTRAL_HEADER}{changed}")] {
            assert_eq!(
                screen::staged_draft_matches(ScreenInput::Styled(&text), "/clear"),
                Some(false)
            );
        }
    }
    // Synthetic: no selected row, two selected rows, a clipped selection, or faint draft.
    for changed in [
        text.replace("  ❯ /clear", "    /clear"),
        text.replace(
            "  ❯ /clear",
            &format!("  ❯ /other{}Other command\n  ❯ /clear", " ".repeat(40)),
        ),
        text.replace("  ❯ /clear", "  ❯ …clear"),
        text.replace("❯\u{a0}/clear", "❯\u{a0}\x1b[2m/clear\x1b[22m"),
        text.replace("❯\u{a0}/clear", "❯\u{a0}/cl\x1b[2mear\x1b[22m"),
    ] {
        assert_ne!(changed, text);
        for text in [changed.clone(), format!("{NEUTRAL_HEADER}{changed}")] {
            assert_eq!(
                screen::staged_draft_matches(ScreenInput::Styled(&text), "/clear"),
                None
            );
            assert_eq!(observe(&text).dialog, ControlDialog::Unknown);
        }
    }
}

#[test]
fn synthetic_duplicate_selected_command_names_refuse_staging() {
    let text = plain(ABOVE_COMPLETIONS.first().unwrap());
    let selected = text
        .lines()
        .find(|row| row.starts_with("  ❯ /clear"))
        .unwrap();
    let skill = text.lines().find(|row| row.starts_with("    …")).unwrap();
    // Synthetic: a user skill shares the selected command's name. Keep the
    // real frame/description columns; neither wording nor order proves identity.
    let custom = selected.replace(
        "Start a new session with empty context; previous session stays",
        "My own clear skill (user)",
    );
    assert_ne!(custom, selected);
    let duplicate = selected.replace("  ❯ /clear", "    /clear");
    assert_eq!(
        screen::staged_draft_matches(
            ScreenInput::Styled(&text.replace(selected, &custom)),
            "/clear"
        ),
        Some(true)
    );
    for changed in [
        text.replace(selected, &custom).replace(skill, &duplicate),
        text.replace(selected, &duplicate).replace(skill, &custom),
    ] {
        for changed in [changed.clone(), changed.replace("/clear", "/other")] {
            for changed in [changed.clone(), format!("{NEUTRAL_HEADER}{changed}")] {
                assert_eq!(
                    screen::staged_draft_matches(ScreenInput::Styled(&changed), "/clear"),
                    None
                );
                assert_eq!(observe(&changed).dialog, ControlDialog::Unknown);
            }
        }
    }
}

#[test]
fn above_completion_requires_exact_frame() {
    // Accepting arbitrary popup rows or weakening the shared frame check must fail.
    let text = plain(ABOVE_COMPLETIONS.first().unwrap());
    let rule = "─".repeat(116);
    let status = text.lines().find(|s| s.starts_with("  [fixture]")).unwrap();
    // Synthetic: each mutation damages one captured boundary, not command identity.
    for (why, changed) in [
        ("missing rule", text.replacen(&rule, "", 1)),
        ("unequal rules", text.replacen(&rule, &"─".repeat(115), 1)),
        (
            "second editor row",
            text.replace("❯\u{a0}/clear", "❯\u{a0}/clear\n  second row"),
        ),
        ("missing editor", text.replace("❯\u{a0}/clear", "")),
        ("changed footer", text.replace("cycle)", "changed)")),
        (
            "missing footer",
            text.replace("  ⏵⏵ auto mode on (shift+tab to cycle)", ""),
        ),
        (
            "second status row",
            text.replace(status, &format!("{status}\n  extra status")),
        ),
        (
            "displaced status",
            text.replace(status, &format!("\n{status}")),
        ),
        (
            "unindented status",
            text.replace(status, status.trim_start()),
        ),
        ("extra suffix", format!("{text}\nUnknown popup")),
        (
            "unknown popup row",
            text.replacen(&rule, &format!("  Unexpected popup row\n{rule}"), 1),
        ),
        (
            "gap before editor",
            text.replacen(&rule, &format!("\n{rule}"), 1),
        ),
        (
            "gap before continuation",
            text.replace(
                "                                                  consistent wording",
                "\n                                                  consistent wording",
            ),
        ),
        (
            "damaged skill margin",
            text.replace("    …ents", "     …ents"),
        ),
        (
            "damaged description margin",
            text.replace(
                "                                                  on disk",
                "                                                 on disk",
            ),
        ),
        ("conceal", format!("{text}\x1b[8m")),
        (
            "unsupported status style",
            text.replace(status, "  \x1b[5mblinking\x1b[25m"),
        ),
    ] {
        assert_ne!(changed, text, "{why}");
        for text in [changed.clone(), format!("{NEUTRAL_HEADER}{changed}")] {
            assert_eq!(
                screen::staged_draft_matches(ScreenInput::Styled(&text), "/clear"),
                None,
                "{why}"
            );
            assert_eq!(observe(&text).dialog, ControlDialog::Unknown, "{why}");
        }
    }
}

#[test]
fn above_completion_values_are_not_input() {
    let text = plain(ABOVE_COMPLETIONS.first().unwrap());
    let status = text.lines().find(|s| s.starts_with("  [fixture]")).unwrap();
    // Synthetic: description/status/history values and Insight rules are not command identity.
    for changed in [
        text.replace(
            "Start a new session with empty context; previous session stays",
            "A different description with /clear in it",
        ),
        text.replace(status, "  /clear"),
        text.replace(&format!("{status}\n"), ""),
        format!("  ★ Insight ─────────────────────────────\n{text}"),
        text.replace("v2.1.325", "v9.8.7"),
    ] {
        for text in [changed.clone(), format!("{NEUTRAL_HEADER}{changed}")] {
            let input = ScreenInput::Styled(&text);
            assert_eq!(screen::staged_draft_matches(input, "/clear"), Some(true));
            let control = observe(&text);
            assert_eq!(control.dialog, ControlDialog::None);
            assert_eq!(control.facts.turn, TurnState::Unknown);
            assert_eq!(control.facts.fresh_editor, FreshEditorEvidence::Unknown);
        }
    }
    let wrong = text.replace("❯\u{a0}/clear", "❯\u{a0}/other");
    let wrong = wrong.replace(status, "  /clear");
    assert_eq!(
        screen::staged_draft_matches(ScreenInput::Styled(&format!("/clear\n{wrong}")), "/clear"),
        Some(false)
    );
    // Synthetic: the grammar is not a /clear spelling exception.
    let other = text.replace("/clear", "/other");
    assert_eq!(
        screen::staged_draft_matches(ScreenInput::Styled(&other), "/other"),
        Some(true)
    );
}

#[test]
fn captured_native_cursor_suggestion_after_a_turn_allows_a_clear() {
    // What `agent clear` needs: no dialog, an empty idle editor, a conversation.
    let after_turn =
        include_str!("fixtures/screen_styled_s1_native-suggestion-after-turn_2.1.325.txt");
    let screen = observe(after_turn);
    assert_eq!(screen.dialog, ControlDialog::None);
    assert!(screen.conversation);
    assert_eq!(screen.facts.input, InputState::Empty);
    assert_eq!(screen.facts.turn, TurnState::NotRunning);
    assert_eq!(screen.facts.fresh_editor, FreshEditorEvidence::Unknown);
    // A never-used editor with the startup placeholder is fresh, with no conversation.
    let fresh = include_str!("fixtures/screen_styled_s1_native-fresh-placeholder_2.1.325.txt");
    let screen = observe(fresh);
    assert_eq!(screen.dialog, ControlDialog::None);
    assert!(!screen.conversation);
    assert_eq!(screen.facts.input, InputState::Empty);
    assert_eq!(screen.facts.turn, TurnState::NotRunning);
    assert_eq!(screen.facts.fresh_editor, FreshEditorEvidence::Present);
}
