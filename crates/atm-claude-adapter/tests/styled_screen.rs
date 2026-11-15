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
    // Synthetic: faint or partly faint command text is not typed input.
    for command in ["\x1b[2m/clear", "/cl\x1b[2mear"] {
        let text = BLANK.replace("\x1b[7m \x1b[0m", &format!("{command}\x1b[0m"));
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&text), "/clear"),
            None
        );
        assert_eq!(
            observe_screen(ScreenInput::Styled(&text), None).input,
            InputState::Unknown
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
