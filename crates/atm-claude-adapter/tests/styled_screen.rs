use atm_claude_adapter::screen::{self, SCREEN_TEXT_FORMAT};
use atm_core::{
    DialogState, FreshEditorEvidence, InputState, ScreenCursor, ScreenFacts, ScreenInput,
    ScreenTextFormat, TurnState,
};

macro_rules! capture {
    ($name:literal) => {
        include_str!(concat!("fixtures/screen_styled_", $name, "_2.1.325.txt"))
    };
}
const FRESH: &str = capture!("u5b_poor-enabled-fresh");
const DRAFT: &str = capture!("u5b_poor-enabled-same-text-draft-cursor-start");
const BLANK: &str = capture!("u5b_poor-enabled-after-clear-settled");

// Fixtures retain provenance headers, which are not terminal rows.
fn body(input: ScreenInput<'_>) -> ScreenInput<'_> {
    match input {
        ScreenInput::Styled(s) if s.starts_with("Captured:") => {
            ScreenInput::Styled(s.split_once('\n').map_or(s, |(_, body)| body))
        }
        other => other,
    }
}
fn observe_screen(input: ScreenInput<'_>, cursor: Option<ScreenCursor>) -> ScreenFacts {
    screen::observe_screen(body(input), cursor)
}
fn staged_draft_matches(input: ScreenInput<'_>, expected: &str) -> Option<bool> {
    screen::staged_draft_matches(body(input), expected)
}
fn facts(input: InputState, turn: TurnState, fresh: FreshEditorEvidence) -> ScreenFacts {
    let mut f = ScreenFacts::default();
    f.input = input;
    f.turn = turn;
    f.dialog = DialogState::None;
    f.fresh_editor = fresh;
    f
}
fn check(text: &str, expected: ScreenFacts) {
    assert_eq!(SCREEN_TEXT_FORMAT, ScreenTextFormat::Styled);
    for cursor in [
        None,
        Some(ScreenCursor { column: 0, row: 0 }),
        Some(ScreenCursor { column: 2, row: 6 }),
    ] {
        assert_eq!(observe_screen(ScreenInput::Styled(text), cursor), expected);
    }
}

const CUSTOM_STATUS: [(&str, &str); 3] = [
    (
        capture!("s1_status-80-fresh"),
        capture!("s1_status-80-draft"),
    ),
    (
        capture!("s1_status-116-fresh"),
        capture!("s1_status-116-draft"),
    ),
    (
        capture!("s1_status-160-fresh"),
        capture!("s1_status-160-draft"),
    ),
];
const REFERENCE: &str = "[m42] Read: atm inbox --id m42";

fn custom_status_plain(fixture: &str) -> String {
    let ScreenInput::Styled(text) = body(ScreenInput::Styled(fixture)) else {
        unreachable!("body preserves the input format")
    };
    atm_core::styled_screen::StyledScreen::parse(text)
        .unwrap_or_else(|error| panic!("invalid fixture styles: {error}"))
        .text()
        .to_owned()
}

// These test-only transforms operate on the known fixtures, not arbitrary UI.
fn custom_status_row(text: &str) -> &str {
    text.lines()
        .find(|line| line.starts_with("  ["))
        .unwrap_or_else(|| panic!("missing status row in fixture"))
}

fn custom_status_free(text: &str) -> String {
    text.replace(&format!("{}\n", custom_status_row(text)), "")
}

#[test]
fn custom_status_real_empty_and_draft() {
    for (fresh, draft) in CUSTOM_STATUS {
        check(
            fresh,
            facts(
                InputState::Empty,
                TurnState::NotRunning,
                FreshEditorEvidence::Present,
            ),
        );
        check(
            draft,
            // The late auto-mode disclaimer in these real drafts is not idle evidence.
            facts(
                InputState::Draft,
                TurnState::Unknown,
                FreshEditorEvidence::Unknown,
            ),
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(fresh), REFERENCE),
            None
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(draft), REFERENCE),
            Some(true)
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(draft), "wrong reference"),
            Some(false)
        );
        for text in [fresh, draft] {
            let plain = custom_status_plain(text);
            // Synthetic: remove only the status row; all editor semantics must agree.
            assert_eq!(
                observe_screen(ScreenInput::Styled(text), None),
                observe_screen(ScreenInput::Styled(&custom_status_free(&plain)), None)
            );
        }
    }
}

#[test]
fn custom_status_colored_and_truncated_rows_are_idle() {
    for (fixture, truncated) in [
        (capture!("s1_status-color-160"), false),
        (capture!("s1_status-color-88"), true),
    ] {
        let expected = facts(
            InputState::Empty,
            TurnState::NotRunning,
            FreshEditorEvidence::Present,
        );
        check(fixture, expected.clone());
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(fixture), REFERENCE),
            None
        );
        let plain = custom_status_plain(fixture);
        assert_eq!(
            custom_status_row(&plain)
                .trim_end_matches(' ')
                .ends_with('…'),
            truncated
        );
        // Synthetic: neither color nor the presence of this status slot changes facts.
        check(&plain, expected.clone());
        check(&custom_status_free(&plain), expected.clone());
        let control = screen::observe_control_screen(body(ScreenInput::Styled(fixture)), None);
        assert_eq!(control.facts, expected);
        assert_eq!(
            control.dialog,
            atm_core::harness_control::ControlDialog::None
        );
    }
}

#[test]
fn custom_status_underline_does_not_change_observations() {
    let plain = custom_status_plain(capture!("s1_status-color-160"));
    let row = custom_status_row(&plain);
    // Synthetic: give the captured one-row status slot underline, and no other attributes.
    let baseline = plain.replace(row, "  status");
    let decorated = plain.replace(row, "  \x1b[4mstatus\x1b[24m");
    let control = |text| screen::observe_control_screen(ScreenInput::Styled(text), None);
    let expected = control(&baseline);
    assert_eq!(expected.facts.input, InputState::Empty);
    assert_eq!(expected.facts.turn, TurnState::NotRunning);
    assert_eq!(control(&decorated), expected);
}

#[test]
fn custom_status_unsupported_decoration_stays_unknown() {
    let plain = custom_status_plain(capture!("s1_status-color-160"));
    let row = custom_status_row(&plain);
    // Synthetic: blink and strikethrough remain unsupported by the decoder.
    for decorated in ["  \x1b[5mblinking\x1b[25m", "  \x1b[9mstruck\x1b[29m"] {
        let text = plain.replace(row, decorated);
        check(&text, ScreenFacts::default());
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&text), REFERENCE),
            None
        );
        assert_eq!(
            screen::observe_control_screen(ScreenInput::Styled(&text), None).dialog,
            atm_core::harness_control::ControlDialog::Unknown
        );
    }
}

#[test]
fn custom_status_rejects_unknown_regions() {
    let fresh = custom_status_plain(capture!("s1_status-160-fresh"));
    let row = custom_status_row(&fresh);
    let rule = "─".repeat(160);
    let mut missing_lower = fresh.clone();
    let start = missing_lower.rfind(&rule).unwrap();
    missing_lower.replace_range(start..start + rule.len(), "");
    // Synthetic: each mutation damages one boundary of the captured one-row slot.
    for (why, text) in [
        (
            "two rows",
            fresh.replace(row, &format!("{row}\n  second status row")),
        ),
        (
            "blank before status",
            fresh.replace(row, &format!("\n{row}")),
        ),
        ("no margin", fresh.replace(row, row.trim_start())),
        (
            "one space",
            fresh.replace(row, row.strip_prefix(' ').unwrap()),
        ),
        ("tab margin", fresh.replace(row, "\tstatus")),
        ("unequal rules", fresh.replacen(&rule, &"─".repeat(159), 1)),
        ("missing lower rule", missing_lower),
        ("changed footer", fresh.replace("cycle)", "changed)")),
        ("unknown footer", format!("{fresh}\nUnknown footer\n")),
        (
            "teammate below footer",
            format!("{fresh}\n  ❯ teammate · idle\n"),
        ),
    ] {
        assert_eq!(
            observe_screen(ScreenInput::Styled(&text), None),
            ScreenFacts::default(),
            "{why}"
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&text), REFERENCE),
            None,
            "{why}"
        );
        assert_eq!(
            screen::observe_control_screen(ScreenInput::Styled(&text), None).dialog,
            atm_core::harness_control::ControlDialog::Unknown,
            "{why}"
        );
    }
}

#[test]
fn custom_status_values_do_not_stage_input() {
    for (fresh, draft) in CUSTOM_STATUS {
        let fresh = custom_status_plain(fresh);
        let draft = custom_status_plain(draft);
        // Synthetic: status values never establish input or staged-text identity.
        for output in [
            "arbitrary text",
            "/clear",
            REFERENCE,
            "❯ fake editor",
            "99% Another Model",
        ] {
            let empty = fresh.replace(custom_status_row(&fresh), &format!("  {output}"));
            check(
                &empty,
                facts(
                    InputState::Empty,
                    TurnState::NotRunning,
                    FreshEditorEvidence::Present,
                ),
            );
            assert_eq!(
                staged_draft_matches(ScreenInput::Styled(&empty), output),
                None
            );
            let wrong = draft.replace(REFERENCE, "different editor draft");
            let wrong = wrong.replace(custom_status_row(&wrong), &format!("  {output}"));
            assert_eq!(
                staged_draft_matches(ScreenInput::Styled(&wrong), output),
                Some(false)
            );
        }
        // Synthetic: retain the existing blank-padding, hint and right-notice grammar.
        let row = custom_status_row(&fresh);
        let footer = fresh
            .lines()
            .find(|line| line.trim_start().starts_with("⏵⏵"))
            .unwrap();
        for text in [
            fresh.replace(row, &format!("{row}   \n   \n")),
            fresh.replace(footer, &format!("{footer}  separate notice")),
            format!("{}\nCtrl+Y to paste deleted text\n", fresh.trim_end()),
        ] {
            check(
                &text,
                facts(
                    InputState::Empty,
                    TurnState::NotRunning,
                    FreshEditorEvidence::Present,
                ),
            );
        }
    }
}

#[test]
fn custom_status_preserves_input_styles_and_multiline_drafts() {
    let fresh = custom_status_plain(capture!("s1_status-160-fresh"));
    // Synthetic: independent of the native-suggestion decision on this branch.
    // The same mutations must retain their semantics when cp/s1-sugg is merged.
    for editor in [
        "\x1b[2mTry \"something\"\x1b[0m",
        "plain \x1b[2mfaint\x1b[0m",
        "\x1b[7mT\x1b[0;2mry \"something\"\x1b[0m",
        "\x1b[2;7mfaint inverse\x1b[0m",
        "first row\n second row",
    ] {
        let text = fresh.replace("❯\u{a0}", &format!("❯\u{a0}{editor}"));
        let twin = custom_status_free(&text);
        let actual = observe_screen(ScreenInput::Styled(&text), None);
        assert_eq!(
            actual,
            observe_screen(ScreenInput::Styled(&twin), None),
            "{editor:?}"
        );
        assert_eq!(actual.turn, TurnState::NotRunning);
        assert_eq!(actual.dialog, DialogState::None);
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&text), REFERENCE),
            None
        );
    }
}

#[test]
fn custom_status_preserves_running_and_conversation() {
    for (fixture, turn) in [
        (
            include_str!("fixtures/screen_control_p1_B-auto-submitted-120x40_2.1.325.txt"),
            TurnState::Running,
        ),
        (
            include_str!("fixtures/screen_control_p1_C1-auto-settled-120x40_2.1.325.txt"),
            TurnState::NotRunning,
        ),
    ] {
        let plain = custom_status_plain(fixture);
        let rule = "─".repeat(120);
        let mut text = plain.clone();
        let at = text.rfind(&format!("{rule}\n")).unwrap() + rule.len() + 1;
        // Synthetic: add the captured kind of status slot to an existing real turn.
        text.insert_str(at, "  [fixture] ctx ▰▱ 7% Demo Model\n");
        let input = ScreenInput::Styled(&text);
        let control = screen::observe_control_screen(input, None);
        assert_eq!(
            control,
            screen::observe_control_screen(ScreenInput::Styled(&plain), None)
        );
        assert_eq!(control.facts.turn, turn);
        assert!(control.conversation);
        assert_eq!(
            observe_screen(input, None),
            observe_screen(ScreenInput::Styled(&plain), None)
        );
        assert_eq!(staged_draft_matches(input, REFERENCE), None);
    }
    for fixture in [
        capture!("p1_A80-default-80x40"),
        capture!("p1_C3-dialog-120x40"),
    ] {
        let original = custom_status_plain(fixture);
        // Synthetic: status-looking text cannot manufacture an editor out of a modal.
        let text = format!("{}\n  [fixture] ctx 7% Demo Model\n", original.trim_end());
        assert_ne!(
            screen::observe_control_screen(ScreenInput::Styled(&text), None).dialog,
            atm_core::harness_control::ControlDialog::None
        );
        assert_ne!(
            observe_screen(ScreenInput::Styled(&text), None).turn,
            TurnState::NotRunning
        );
    }
}

#[test]
fn captured_placeholder_and_home_draft_differ() {
    for text in [
        FRESH,
        capture!("u5b_rich-enabled-fresh"),
        capture!("u5b_screen-with-colorterm-enabled-fresh"),
    ] {
        check(
            text,
            facts(
                InputState::Empty,
                TurnState::NotRunning,
                FreshEditorEvidence::Present,
            ),
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(text), "/clear"),
            None
        );
    }
    for text in [
        DRAFT,
        capture!("u5b_rich-enabled-same-text-draft-cursor-start"),
    ] {
        check(
            text,
            facts(
                InputState::Draft,
                TurnState::NotRunning,
                FreshEditorEvidence::Unknown,
            ),
        );
    }
    assert_eq!(
        staged_draft_matches(
            ScreenInput::Styled(DRAFT),
            "Try \"create a util logging.py that...\""
        ),
        Some(true)
    );
}

#[test]
fn blank_padding_collision_stays_empty_but_staged_leading_spaces_refuse() {
    for text in [BLANK, capture!("u5b_rich-enabled-after-clear-settled")] {
        check(
            text,
            facts(
                InputState::Empty,
                TurnState::NotRunning,
                FreshEditorEvidence::Present,
            ),
        );
    }
    // Synthetic: authored spaces and grid padding are indistinguishable, even at Home.
    let blank = BLANK.replace("\x1b[7m \x1b[0m", "\x1b[7m \x1b[0m     ");
    check(
        &blank,
        facts(
            InputState::Empty,
            TurnState::NotRunning,
            FreshEditorEvidence::Present,
        ),
    );
    for leading in [" ", "     ", "\u{a0}"] {
        let staged = BLANK.replace(
            "\x1b[7m \x1b[0m",
            &format!("{leading}/clear\x1b[7m \x1b[0m     "),
        );
        check(
            &staged,
            facts(
                InputState::Draft,
                TurnState::NotRunning,
                FreshEditorEvidence::Unknown,
            ),
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&staged), "/clear"),
            Some(false)
        );
    }
    // Synthetic: trailing ASCII padding is chrome; leading spaces remain exact.
    let exact = BLANK.replace("\x1b[7m \x1b[0m", "/clear");
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(&exact), "/clear"),
        Some(true)
    );
    assert_eq!(
        staged_draft_matches(
            ScreenInput::Styled(&exact.replace("❯\u{a0}/clear", "❯\u{a0}/clear   ")),
            "/clear"
        ),
        Some(true)
    );
    assert_eq!(
        staged_draft_matches(ScreenInput::Plain(&exact), "/clear"),
        None
    );
}

#[test]
fn captured_dialogs_and_unsupported_modes() {
    for text in [
        capture!("p1_A80-default-80x40"),
        capture!("p1_A120-default-120x40"),
        capture!("p1_A200-default-200x40"),
    ] {
        let mut expected = ScreenFacts::default();
        expected.dialog = DialogState::FolderTrust {
            folder: "/tmp/xxxxxxxxxxxxxxxxxxxxxxx/cwd".into(),
        };
        check(text, expected);
    }
    for text in [
        capture!("p1_A80-selected-80x40"),
        capture!("p1_B-clear-007-120x40"),
        capture!("p1_C3-dialog-120x40"),
    ] {
        check(text, ScreenFacts::default());
    }
    // Shell launch line is not startup evidence, even though the auto editor is recognized.
    check(
        capture!("p1_A-auto-ready-120x40"),
        facts(
            InputState::Empty,
            TurnState::Unknown,
            FreshEditorEvidence::Unknown,
        ),
    );
}

#[test]
fn real_retained_padding_matches_staged_text() {
    let text = capture!("p1_B-auto-pasted-120x40");
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(text), "B_AUTO_EVENT_SENTINEL"),
        Some(true)
    );
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(text), "B_AUTO_EVENT_SENTINEL_WRONG"),
        Some(false)
    );
}

#[test]
fn real_slash_menu_and_manual_queued_frame_remain_unknown() {
    for (text, expected) in [
        (capture!("p1_B-clear-001-120x40"), "/clear"),
        (
            capture!("p1_C2-queued-001-120x40"),
            "Press up to edit queued messages",
        ),
    ] {
        check(text, ScreenFacts::default());
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(text), expected),
            if expected == "/clear" {
                Some(true)
            } else {
                None
            }
        );
    }
}

#[test]
fn faint_text_is_not_a_typed_draft() {
    // Synthetic: faint or partly faint command text is not typed input. A row
    // that is faint throughout is a suggestion; a partly faint one is unknown.
    for (command, input) in [
        ("\x1b[2m/clear", InputState::Empty),
        ("/cl\x1b[2mear", InputState::Unknown),
    ] {
        let text = BLANK.replace("\x1b[7m \x1b[0m", &format!("{command}\x1b[0m"));
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&text), "/clear"),
            None
        );
        assert_eq!(
            observe_screen(ScreenInput::Styled(&text), None).input,
            input
        );
    }
}

#[test]
fn captured_queued_placeholder_row_in_auto_frame_is_empty() {
    let queued = capture!("p1_C2-queued-001-120x40");
    let row = queued
        .lines()
        .find(|line| line.contains("ress up to edit queued messages"))
        .unwrap();
    // Synthetic: transplant the captured manual-mode placeholder row into supported
    // auto-mode chrome; manual-mode parsing belongs to the control consumer's slice.
    let text = BLANK
        .lines()
        .map(|line| {
            if line.contains("❯\u{a0}") {
                row
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    check(
        &text,
        facts(
            InputState::Empty,
            TurnState::NotRunning,
            FreshEditorEvidence::Present,
        ),
    );
    assert_eq!(
        staged_draft_matches(
            ScreenInput::Styled(&text),
            "Press up to edit queued messages"
        ),
        None
    );
    // Synthetic: removing faint styling makes the identical visible text a draft.
    let typed = text.replace("[0;2mress", "[0mress");
    assert_ne!(typed, text);
    assert_eq!(
        staged_draft_matches(
            ScreenInput::Styled(&typed),
            "Press up to edit queued messages"
        ),
        Some(true)
    );
    // Synthetic: partial faint evidence must not turn mixed text into Empty or Draft.
    let mixed = text.replace("edit queued", "edit\x1b[22m queued");
    assert_eq!(
        observe_screen(ScreenInput::Styled(&mixed), None).input,
        InputState::Unknown
    );
    assert_eq!(
        staged_draft_matches(
            ScreenInput::Styled(&mixed),
            "Press up to edit queued messages"
        ),
        None
    );
}

#[test]
fn non_ascii_trailing_whitespace_is_not_padding() {
    // Synthetic: only ASCII spaces are chrome; other trailing whitespace stays distinct.
    for suffix in ["\u{a0}", "\u{2003}"] {
        let expected = format!("/clear{suffix}");
        let text = BLANK.replace("\x1b[7m \x1b[0m", &expected);
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&text), &expected),
            None
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&text), "/clear"),
            Some(false)
        );
    }
}

#[test]
fn expected_trailing_spaces_cannot_turn_padding_into_exactness() {
    // Synthetic: expected text could accidentally equal the renderer's padding.
    let padded = BLANK.replace("\x1b[7m \x1b[0m", "/clear   ");
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(&padded), "/clear   "),
        None
    );
}

#[test]
fn placeholder_first_cell_requires_exact_intensity() {
    // Synthetic: faint + inverse on T is not the captured placeholder evidence.
    let faint_cursor = FRESH.replace("[7mT", "[2;7mT");
    assert_ne!(
        observe_screen(ScreenInput::Styled(&faint_cursor), None).input,
        InputState::Empty
    );
    check(
        capture!("u5b_rich-enabled-same-text-draft"),
        ScreenFacts::default(),
    );
}

#[test]
fn synthetic_style_and_frame_mutations_never_create_empty() {
    // Synthetic: partial faint, missing inverse, hidden and malformed text, changed/cropped chrome.
    for text in [
        FRESH.replace("0;2mry", "0mry"),
        FRESH.replace("[7mT", "[0mT"),
        format!("{FRESH}\x1b[8m"),
        format!("{FRESH}\x1b["),
        FRESH.replace("cycle)", "changed)"),
        FRESH.replace(&format!("{}\n", "─".repeat(100)), "\n"),
    ] {
        assert_ne!(
            observe_screen(ScreenInput::Styled(&text), None).input,
            InputState::Empty
        );
        assert_ne!(
            staged_draft_matches(ScreenInput::Styled(&text), "/clear"),
            Some(true)
        );
    }
    let changed = FRESH
        .replace("logging.py", "other.rs")
        .replace("v2.1.325", "v9.8.7")
        .replace("[31m", "[38;2;2;7;0m");
    check(
        &changed,
        facts(
            InputState::Empty,
            TurnState::NotRunning,
            FreshEditorEvidence::Present,
        ),
    );
    let extra = format!("{}\nUnknown footer\n", FRESH.trim_end());
    check(&extra, ScreenFacts::default());
    // Synthetic: multiline draft cannot be reconstructed as a single line.
    let multi = BLANK.replace("\x1b[7m \x1b[0m", "first\n second");
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(&multi), "first\n second"),
        None
    );
    check("\x1b[7m \x1b[0m", ScreenFacts::default());
}

#[test]
fn above_completion_preserves_legacy_draft_facts() {
    // Captured: Claude Code 2.1.325, 2026-11-15, sealed tmux capture-pane -e, no prompt.
    // Recognizing a completion must not turn its unknown transcript tail into idle.
    for text in [
        capture!("s1_clear-multi-wrapped"),
        capture!("s1_clear-single"),
    ] {
        check(
            text,
            facts(
                InputState::Draft,
                TurnState::Unknown,
                FreshEditorEvidence::Unknown,
            ),
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(text), "/clear"),
            Some(true)
        );
    }
}

const SUGG2_TEXT: &str =
    "check the captured suggestion after resizing the terminal to a narrow width";
const SUGG2_DRAWN: &str = capture!("s1_sugg2-drawn-low-suggestion");
const SUGG2_DRAWN_ROW: &str = "\x1b[7mc\x1b[0;2mheck the captured suggestion after resizing the terminal to a narrow width\x1b[0m";

#[test]
fn captured_drawn_cursor_suggestions_are_empty_and_never_staged() {
    for text in [
        SUGG2_DRAWN,
        capture!("s1_sugg2-drawn-low-narrow-truncated-suggestion"),
        capture!("s1_sugg2-drawn-low-classic-suggestion"),
        capture!("s1_sugg2-native-low-suggestion"),
        capture!("s1_sugg2-native-low-narrow-truncated-suggestion"),
        capture!("s1_sugg2-native-low-classic-suggestion"),
    ] {
        check(
            text,
            facts(
                InputState::Empty,
                TurnState::NotRunning,
                FreshEditorEvidence::Unknown,
            ),
        );
        let control = screen::observe_control_screen(body(ScreenInput::Styled(text)), None);
        assert_eq!(
            control.dialog,
            atm_core::harness_control::ControlDialog::None
        );
        assert!(control.conversation);
        for expected in [SUGG2_TEXT, "/clear"] {
            assert_eq!(
                staged_draft_matches(ScreenInput::Styled(text), expected),
                None
            );
        }
    }
}

#[test]
fn drawn_suggestion_mutations_never_hide_authored_text() {
    assert!(SUGG2_DRAWN.contains(SUGG2_DRAWN_ROW));
    // Synthetic: change the captured cursor/suffix evidence, not the editor frame.
    for row in [
        "c\x1b[2mheck the suggestion\x1b[0m",
        "\x1b[2;7mc\x1b[0;2mheck the suggestion\x1b[0m",
        "\x1b[7mc\x1b[0;2mheck \x1b[7mthe\x1b[27m suggestion\x1b[0m",
        "\x1b[7mc\x1b[0;2mheck the\x1b[0m suggestion",
        "check \x1b[2mthe\x1b[0m suggestion",
        " \x1b[7mc\x1b[0;2mheck the suggestion\x1b[0m",
        "\u{a0}\x1b[7mc\x1b[0;2mheck the suggestion\x1b[0m",
        // The cursor is over leading padding, not the first non-space glyph.
        "\x1b[7m \x1b[0mc\x1b[2mheck the suggestion\x1b[0m",
        "\x1b[2;7mx\x1b[0m",
    ] {
        let text = SUGG2_DRAWN.replace(SUGG2_DRAWN_ROW, row);
        assert_eq!(
            observe_screen(ScreenInput::Styled(&text), None).input,
            InputState::Unknown,
            "{row:?}"
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&text), SUGG2_TEXT),
            None,
            "{row:?}"
        );
    }
    // Synthetic: identical visible text without faint styling is authored input,
    // including a single glyph under a drawn cursor and non-ASCII typed text.
    for (row, expected) in [
        (SUGG2_TEXT, SUGG2_TEXT),
        ("\x1b[7mc\x1b[0mheck the captured suggestion after resizing the terminal to a narrow width", SUGG2_TEXT),
        ("\x1b[7mx\x1b[0m", "x"),
        ("\x1b[7m界\x1b[0mé typed", "界é typed"),
    ] {
        let text = SUGG2_DRAWN.replace(SUGG2_DRAWN_ROW, row);
        assert_eq!(observe_screen(ScreenInput::Styled(&text), None).input, InputState::Draft, "{row:?}");
        assert_eq!(staged_draft_matches(ScreenInput::Styled(&text), expected), Some(true), "{row:?}");
    }
    // Synthetic: filtering whitespace must not make a vacuous suffix prove empty.
    for row in ["\u{a0}", "\u{3000}", "\x1b[7mx\x1b[0m   "] {
        let text = SUGG2_DRAWN.replace(SUGG2_DRAWN_ROW, row);
        assert_eq!(
            observe_screen(ScreenInput::Styled(&text), None).input,
            InputState::Draft,
            "{row:?}"
        );
    }
    // Synthetic: UTF-8 character starts, not byte-count cursor coordinates.
    let unicode = SUGG2_DRAWN.replace(SUGG2_DRAWN_ROW, "\x1b[7m界\x1b[0;2mé suggestion\x1b[0m");
    assert_eq!(
        observe_screen(ScreenInput::Styled(&unicode), None).input,
        InputState::Empty
    );
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(&unicode), "界é suggestion"),
        None
    );
    // Synthetic: a second row remains a draft even when both rows look faint.
    let multi = SUGG2_DRAWN.replace(
        SUGG2_DRAWN_ROW,
        &format!("{SUGG2_DRAWN_ROW}\n  \x1b[2mcontinued\x1b[0m"),
    );
    assert_eq!(
        observe_screen(ScreenInput::Styled(&multi), None).input,
        InputState::Draft
    );
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(&multi), SUGG2_TEXT),
        None
    );
}

const SUGG2_HINT: &str = capture!("s1_sugg2-native-high-hint-only-after-three-turns");

#[test]
fn captured_clear_context_hint_does_not_mask_turn_completion() {
    for text in [
        SUGG2_HINT,
        capture!("s1_sugg2-native-high-hint-only-80"),
        capture!("s1_sugg2-native-high-hint-only-160"),
        capture!("s1_sugg2-drawn-high-hint-only-after-three-turns"),
        capture!("s1_sugg2-drawn-high-hint-only-80"),
        capture!("s1_sugg2-drawn-high-hint-only-160"),
        capture!("s1_sugg2-native-high-hint-and-suggestion"),
        capture!("s1_sugg2-drawn-high-hint-and-suggestion"),
    ] {
        check(
            text,
            facts(
                InputState::Empty,
                TurnState::NotRunning,
                FreshEditorEvidence::Unknown,
            ),
        );
        for expected in ["/clear", SUGG2_TEXT] {
            assert_eq!(
                staged_draft_matches(ScreenInput::Styled(text), expected),
                None
            );
        }
    }
}

#[test]
fn clear_hint_preserves_real_typed_drafts_and_unknown_tails() {
    for text in [
        capture!("s1_sugg2-native-high-typed-control"),
        capture!("s1_sugg2-native-high-typed-home-control"),
        capture!("s1_sugg2-drawn-high-typed-control"),
        capture!("s1_sugg2-drawn-high-typed-home-control"),
    ] {
        check(
            text,
            facts(
                InputState::Draft,
                TurnState::NotRunning,
                FreshEditorEvidence::Unknown,
            ),
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(text), "literal typed draft"),
            Some(true)
        );
        for expected in ["/clear", SUGG2_TEXT] {
            assert_eq!(
                staged_draft_matches(ScreenInput::Styled(text), expected),
                Some(false)
            );
        }
    }
    for text in [
        capture!("s1_sugg2-native-high-hint-only"),
        capture!("s1_sugg2-drawn-high-hint-only"),
    ] {
        // The real first-turn auto-mode explanation still follows the done row.
        check(
            text,
            facts(
                InputState::Empty,
                TurnState::Unknown,
                FreshEditorEvidence::Unknown,
            ),
        );
    }
    for text in [
        capture!("s1_sugg2-native-low-wrapped-typed-control"),
        capture!("s1_sugg2-drawn-low-wrapped-typed-control"),
    ] {
        check(
            text,
            facts(
                InputState::Draft,
                TurnState::NotRunning,
                FreshEditorEvidence::Unknown,
            ),
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(text), SUGG2_TEXT),
            None
        );
    }
    for text in [
        capture!("s1_sugg2-native-low-classic-narrow-truncated-suggestion"),
        capture!("s1_sugg2-drawn-low-classic-narrow-truncated-suggestion"),
        capture!("s1_sugg2-native-low-classic-wrapped-typed-control"),
        capture!("s1_sugg2-drawn-low-classic-wrapped-typed-control"),
    ] {
        // The extra effort row below the classic footer is still unsupported.
        check(text, ScreenFacts::default());
        assert_eq!(
            screen::observe_control_screen(body(ScreenInput::Styled(text)), None).dialog,
            atm_core::harness_control::ControlDialog::Unknown
        );
    }
    for text in [
        capture!("s1_sugg2-native-low-typed-control"),
        capture!("s1_sugg2-drawn-low-typed-control"),
    ] {
        // Ctrl+U left a first-row draft and an unsupported above-border Ctrl+Y notice.
        let observed = observe_screen(ScreenInput::Styled(text), None);
        assert_eq!(observed.input, InputState::Draft);
        assert_eq!(observed.turn, TurnState::Unknown);
    }
}

fn sugg2_hint_row(text: &str) -> &str {
    text.lines()
        .find(|row| row.contains("new task? /clear to save"))
        .unwrap_or_else(|| panic!("missing clear-context hint in fixture"))
}

fn sugg2_aligned_hint(count: &str) -> String {
    format!(
        "{:>114}",
        format!("new task? /clear to save {count} tokens")
    )
}

#[test]
fn clear_hint_compact_values_preserve_input() {
    // Synthetic: keep the real 116-column slot/alignment while varying its value.
    let text = custom_status_plain(SUGG2_HINT);
    for count in ["100k", "355k", "1.2m", "999", "1b", "2.3t"] {
        let changed = text.replace(sugg2_hint_row(&text), &sugg2_aligned_hint(count));
        check(
            &changed,
            facts(
                InputState::Empty,
                TurnState::NotRunning,
                FreshEditorEvidence::Unknown,
            ),
        );
        // The same hint-looking text, actually authored in the editor, is a draft.
        let literal = format!("new task? /clear to save {count} tokens");
        let draft = changed.replace("❯\u{a0}", &format!("❯\u{a0}{literal}"));
        check(
            &draft,
            facts(
                InputState::Draft,
                TurnState::NotRunning,
                FreshEditorEvidence::Unknown,
            ),
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&draft), &literal),
            Some(true)
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&draft), "/clear"),
            Some(false)
        );
    }
}

#[test]
fn clear_hint_mutations_never_manufacture_idle() {
    let text = custom_status_plain(SUGG2_HINT);
    let hint = sugg2_hint_row(&text);
    // Synthetic: noncanonical/unrecognized values cannot hide unknown history.
    for count in [
        "0",
        "01k",
        "+155k",
        "-155k",
        "1.0m",
        "1.23m",
        "155K",
        "1e5",
        "1,000k",
        "NaN",
        "155k extra",
    ] {
        let changed = text.replace(hint, &sugg2_aligned_hint(count));
        assert_eq!(
            observe_screen(ScreenInput::Styled(&changed), None).turn,
            TurnState::Unknown,
            "{count}"
        );
    }
    // Synthetic: location, shape and frame bounds are not optional notice metadata.
    for (why, changed) in [
        ("wrong prose", text.replace("new task?", "new work?")),
        (
            "wrong command",
            text.replace("/clear to save", "/other to save"),
        ),
        ("wrong unit", text.replace("155k tokens", "155k items ")),
        (
            "right margin three",
            text.replace(hint, hint.trim_end_matches(' ').strip_prefix(' ').unwrap()),
        ),
        (
            "right margin one",
            text.replace(hint, &format!(" {}", hint.trim_end_matches(' '))),
        ),
        ("no indentation", text.replace(hint, hint.trim())),
        ("displaced hint", text.replace(hint, &format!("{hint}\n"))),
        (
            "second hint",
            text.replace(hint, &format!("{hint}\n{hint}")),
        ),
        (
            "unknown tail",
            text.replace(hint, &format!("{hint}\nUnknown active operation")),
        ),
        ("missing upper rule", text.replacen(&"─".repeat(116), "", 1)),
        ("unknown footer", text.replace("cycle)", "changed)")),
    ] {
        assert_ne!(changed, text, "{why}");
        assert_eq!(
            observe_screen(ScreenInput::Styled(&changed), None).turn,
            TurnState::Unknown,
            "{why}"
        );
        assert_ne!(
            staged_draft_matches(ScreenInput::Styled(&changed), "/clear"),
            Some(true),
            "{why}"
        );
    }
    let done = text
        .lines()
        .rev()
        .find(|row| row.contains(" · done "))
        .unwrap();
    let at = text.rfind(done).unwrap();
    for (status, expected) in [
        ("✻ Generating…", TurnState::Running),
        ("Unknown active operation", TurnState::Unknown),
    ] {
        // Synthetic: a valid hint must never replace the actual status decision.
        let mut changed = text.clone();
        changed.replace_range(at..at + done.len(), status);
        assert_eq!(
            observe_screen(ScreenInput::Styled(&changed), None).turn,
            expected
        );
    }
}

// Native-cursor rendering: Claude shows the terminal's own cursor and draws no
// inverse cell, so a suggestion row is faint from its first cell on.
const NATIVE_FRESH: &str = capture!("s1_native-fresh-placeholder");
const NATIVE_SUGGESTION: &str = capture!("s1_native-suggestion-after-turn");
const SUGGESTION_ROW: &str = "\x1b[2mread the attached note.txt\x1b[0m";

#[test]
fn captured_native_cursor_suggestions_are_empty() {
    check(
        NATIVE_FRESH,
        facts(
            InputState::Empty,
            TurnState::NotRunning,
            FreshEditorEvidence::Present,
        ),
    );
    // A suggestion after a turn has free text, not the `Try "…"` shape.
    assert!(NATIVE_SUGGESTION.contains(SUGGESTION_ROW));
    check(
        NATIVE_SUGGESTION,
        facts(
            InputState::Empty,
            TurnState::NotRunning,
            FreshEditorEvidence::Unknown,
        ),
    );
    for text in [NATIVE_FRESH, NATIVE_SUGGESTION] {
        // A suggestion is never a staged draft, whatever text it shows.
        for expected in [
            "/clear",
            "read the attached note.txt",
            "Try \"how do I log an error?\"",
        ] {
            assert_eq!(
                staged_draft_matches(ScreenInput::Styled(text), expected),
                None
            );
        }
    }
}

#[test]
fn captured_native_cursor_typed_text_is_a_draft() {
    for (text, typed) in [
        (capture!("s1_native-typed-first-letter"), "T"),
        (capture!("s1_native-typed-abc"), "abc"),
        // The placeholder's own text, typed, with the cursor moved to its start.
        (
            capture!("s1_native-same-text-draft-home"),
            "Try \"edit <filepath> to...\"",
        ),
    ] {
        check(
            text,
            facts(
                InputState::Draft,
                TurnState::NotRunning,
                FreshEditorEvidence::Unknown,
            ),
        );
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(text), typed),
            Some(true)
        );
    }
}

#[test]
fn synthetic_mutations_of_a_native_cursor_suggestion_are_never_empty() {
    let rule = format!("{}\n", "─".repeat(149));
    assert!(NATIVE_SUGGESTION.contains(&rule));
    // Synthetic: each row below differs from the captured suggestion in one way.
    for (name, text) in [
        (
            "typed text after the suggestion",
            NATIVE_SUGGESTION.replace(SUGGESTION_ROW, &format!("{SUGGESTION_ROW} ok")),
        ),
        (
            "a faint word inside typed text",
            NATIVE_SUGGESTION.replace(SUGGESTION_ROW, "read \x1b[2mthe\x1b[0m attached note.txt"),
        ),
        (
            "a plain first cell before a faint rest",
            NATIVE_SUGGESTION.replace(SUGGESTION_ROW, "r\x1b[2mead the attached note.txt\x1b[0m"),
        ),
        (
            "a faint cell that is also inverse",
            NATIVE_SUGGESTION.replace(
                SUGGESTION_ROW,
                "\x1b[2;7mr\x1b[0;2mead the attached note.txt\x1b[0m",
            ),
        ),
        (
            "an inverse cell inside the faint text",
            NATIVE_SUGGESTION.replace(
                SUGGESTION_ROW,
                "\x1b[2mread the \x1b[2;7ma\x1b[0;2mttached note.txt\x1b[0m",
            ),
        ),
        (
            "a second row in the editor box",
            NATIVE_SUGGESTION.replace(
                SUGGESTION_ROW,
                &format!("{SUGGESTION_ROW}\n  \x1b[2mand more\x1b[0m"),
            ),
        ),
        (
            "a changed footer",
            NATIVE_SUGGESTION.replace("cycle)", "changed)"),
        ),
        ("a missing rule", NATIVE_SUGGESTION.replacen(&rule, "\n", 1)),
        ("a hidden attribute", format!("{NATIVE_SUGGESTION}\x1b[8m")),
    ] {
        assert_ne!(text, NATIVE_SUGGESTION, "{name}");
        assert_ne!(
            observe_screen(ScreenInput::Styled(&text), None).input,
            InputState::Empty,
            "{name}"
        );
        for expected in ["/clear", "read the attached note.txt"] {
            assert_ne!(
                staged_draft_matches(ScreenInput::Styled(&text), expected),
                Some(true),
                "{name}"
            );
        }
    }
    // Synthetic: the same text without the faint attribute is a typed draft.
    let typed = NATIVE_SUGGESTION.replace(SUGGESTION_ROW, "read the attached note.txt");
    assert_ne!(typed, NATIVE_SUGGESTION);
    assert_eq!(
        observe_screen(ScreenInput::Styled(&typed), None).input,
        InputState::Draft
    );
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(&typed), "read the attached note.txt"),
        Some(true)
    );
}
