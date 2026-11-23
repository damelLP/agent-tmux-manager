use atm_claude_adapter::screen::{
    inbox_survey_ready, observe_control_screen, observe_screen, staged_draft_matches,
};
use atm_core::harness_control::ControlDialog;
use atm_core::styled_screen::StyledScreen;
use atm_core::{DialogState, FreshEditorEvidence, InputState, ScreenFacts, ScreenInput, TurnState};

// Captured: Claude Code not recorded, not recorded, styled tmux viewport (sanitized).
const CAPTURE: &str =
    include_str!("fixtures/claude_background_agent_list_version-not-recorded.styled.txt");
const SGR_WAITING: &str = "\x1b[38;5;246m✻\x1b[39m \x1b[38;5;246mWaiting for \x1b[1m1\x1b[0m\x1b[38;5;246m background agent to finish\x1b[39m";
const REFERENCE: &str = "[m1] Read: atm inbox --id m1";
const MAIN: &str = "  \x1b[1m● main\x1b[0m";
const DURATION: &str = "1m 23s · ↓ 123.4k tokens";

fn frame() -> &'static str {
    match CAPTURE.split_once('\n') {
        Some((_, frame)) => frame,
        None => panic!("missing captured viewport"),
    }
}

fn held(text: &str, input_state: InputState, turn: TurnState) {
    let styled = ScreenInput::Styled(text);
    let facts = observe_screen(styled, None);
    assert_eq!(
        (
            &facts.input,
            &facts.turn,
            &facts.dialog,
            &facts.fresh_editor
        ),
        (
            &input_state,
            &turn,
            &DialogState::None,
            &FreshEditorEvidence::Unknown
        ),
    );
    let control = observe_control_screen(styled, None);
    assert_eq!(control.facts, facts);
    assert_eq!(control.dialog, ControlDialog::Unknown);
    assert_eq!(staged_draft_matches(styled, REFERENCE), None);
    assert!(!inbox_survey_ready(styled));
}

#[test]
fn recorded_agent_list_reads_editor_but_does_not_authorize_input() {
    held(frame(), InputState::Empty, TurnState::Unknown);
}

#[test]
fn background_agent_list_is_only_recognized_as_the_complete_styled_suffix() {
    let plain = StyledScreen::parse(frame()).unwrap();
    assert_eq!(
        observe_screen(ScreenInput::Plain(plain.text()), None),
        ScreenFacts::default()
    );
    let row = frame().lines().last().unwrap();
    // Synthetic: alter just one anchor, attribute, or field in the capture.
    for (from, to) in [
        ("\n\n  \x1b[1m", "\n  \x1b[1m"),
        ("\n\n  \x1b[1m", "\n\n\n  \x1b[1m"),
        ("\x1b[1m● main", "● main"),
        ("\x1b[1m● main", "\x1b[1m● \x1b[22mmain"),
        ("◯ general-purpose", "\x1b[1m◯ general-purpose\x1b[22m"),
        ("◯ general-purpose", "\x1b[7m◯ general-purpose\x1b[27m"),
        ("◯ general-purpose", "● general-purpose"),
        ("\n  \x1b[1m● main\x1b[0m", ""),
        (row, ""),
        ("↓ to manage", "↓ to manage more"),
        (" · ↓ to manage", ""),
        ("  \x1b[38;5;220m", "   \x1b[38;5;220m"),
        (DURATION, "1m · ↓ 123.4k tokens"),
        (DURATION, "1m 23s · ↓ 123.4k tokens extra"),
        (DURATION, "1m 23s · ↓ 123.45k tokens"),
        ("Reading fixture input for local work only.", "Reading "),
        (
            "Reading fixture input for local work only.",
            "Searching fixture input",
        ),
        ("❯\u{a0}", "? "),
    ] {
        assert!(frame().contains(from), "not a mutation: {from:?}");
        let text = frame().replacen(from, to, 1);
        let input = ScreenInput::Styled(&text);
        assert_eq!(
            observe_screen(input, None),
            ScreenFacts::default(),
            "{from:?} → {to:?}"
        );
        assert_eq!(
            observe_control_screen(input, None).dialog,
            ControlDialog::Unknown
        );
        assert_eq!(staged_draft_matches(input, REFERENCE), None);
        assert!(!inbox_survey_ready(input));
    }
    let suffix = format!("{MAIN}\n{row}\n");
    let prefix = frame().strip_suffix(&suffix).unwrap();
    for text in [
        format!("{prefix}{row}\n{MAIN}\n"),
        format!("{}{row}\n", frame()),
        format!("{}\n  unexpected tail\n", frame()),
        frame().lines().skip(33).collect::<Vec<_>>().join("\n"),
    ] {
        let input = ScreenInput::Styled(&text);
        assert_eq!(observe_screen(input, None), ScreenFacts::default());
        assert_eq!(
            observe_control_screen(input, None).dialog,
            ControlDialog::Unknown
        );
    }
}

#[test]
fn an_open_list_is_not_active_editor_permission() {
    // Synthetic: a completed spinner, draft, and exact survey surround the
    // same open list. None may authorize input while the list owns the suffix.
    let done = frame().replace(SGR_WAITING, "✻ Brewed for 19s · done 1000 tokens");
    held(&done, InputState::Empty, TurnState::Unknown);
    let draft = done.replace("❯\u{a0}", &format!("❯\u{a0}{REFERENCE}"));
    held(&draft, InputState::Draft, TurnState::Unknown);
    let survey = done.replace(
        "✻ Brewed for 19s · done 1000 tokens",
        "✻ Brewed for 19s · done 1000 tokens\n● How is Claude doing this session? (optional)\n  1: Bad    2: Fine   3: Good   0: Dismiss",
    );
    held(&survey, InputState::Empty, TurnState::Unknown);
}

#[test]
fn background_agent_list_vetoes_an_otherwise_usable_completion() {
    // Synthetic: an exact above-editor completion on an otherwise identical
    // draft. Remove the aligned notice so the whole block is a valid menu.
    let (_, border_and_body) = frame().split_once("\n\x1b[38;5;244m").unwrap();
    let menu = format!("  ❯ /clear  Clear conversation\n\x1b[38;5;244m{border_and_body}")
        .replace("❯\u{a0}", "❯\u{a0}/clear");
    let (before_hint, _) = menu.split_once(" · ↓ to manage").unwrap();
    let ordinary = format!("{before_hint}\x1b[0m\n");
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(&ordinary), "/clear"),
        Some(true)
    );
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(&menu), "/clear"),
        None
    );
    assert_eq!(
        observe_control_screen(ScreenInput::Styled(&menu), None).dialog,
        ControlDialog::Unknown
    );
}

#[test]
fn background_agent_list_does_not_prove_a_fresh_editor() {
    // Synthetic: a startup header and otherwise fresh editor with the list open.
    let text = format!(
        " ▐▛███▛█   Claude Code v9.8.7\n▝▜██████▀  Fixture model\n ▝▝   ▝▝   ~\n{}",
        frame().replace(SGR_WAITING, "")
    );
    let (before_hint, _) = text.split_once(" · ↓ to manage").unwrap();
    let ordinary = format!("{before_hint}\x1b[0m\n");
    assert_eq!(
        observe_screen(ScreenInput::Styled(&ordinary), None).fresh_editor,
        FreshEditorEvidence::Present
    );
    held(&text, InputState::Empty, TurnState::Unknown);
}

#[test]
fn background_agent_list_does_not_erase_independent_running_evidence() {
    let running = include_str!("fixtures/claude_shells_busy_dim_2.1.327.styled.txt")
        .lines()
        .find(|row| row.contains("Julienning…"))
        .unwrap()
        .trim_end_matches(' ');
    // Synthetic: transplant a independently captured running spinner.
    held(
        &frame().replace(SGR_WAITING, running),
        InputState::Empty,
        TurnState::Running,
    );
}

#[test]
fn background_agent_list_in_scrollback_does_not_own_the_editor() {
    // Synthetic: the old styled list is now above a completed turn and an
    // ordinary footer. Its presence in history must not hold delivery.
    let (history, _) = frame().split_once(SGR_WAITING).unwrap();
    let (_, editor) = frame().split_once(SGR_WAITING).unwrap();
    let (editor, tail) = editor.split_once(" · ↓ to manage").unwrap();
    let text =
        format!("{history}{tail}\u{1b}[0m\n✻ Brewed for 19s · done 1000 tokens{editor}\x1b[0m\n");
    let control = observe_control_screen(ScreenInput::Styled(&text), None);
    assert_eq!(control.facts.input, InputState::Empty);
    assert_eq!(control.facts.turn, TurnState::NotRunning);
    assert_eq!(control.dialog, ControlDialog::None);
}
