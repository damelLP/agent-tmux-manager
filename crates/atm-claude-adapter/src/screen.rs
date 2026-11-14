//! Conservative screen observations for Claude Code 2.1.325.
//!
//! Captured: Claude Code 2.1.325, 2026-11-14, U5 private-tmux capture; sanitized.
//! See `tests/fixtures/README.md` for the complete source mapping and missing states.
//! Input is the live viewport printed by `capture-pane -p`, without attributes or
//! scrollback. Trailing blank rows may be removed. Any control except newline
//! refuses the whole observation: terminal escapes could change any visible fact.
//!
//! Startup suggestions cannot be distinguished from identical drafts in plain
//! text. They remain Unknown: consumers cannot deliver into a freshly started
//! Claude editor until a later evidence-backed implementation resolves this.
//! U5b also captured cursors and confirmed that Home-position drafts collide with
//! empty editors. Supplying a cursor with Plain returns all Unknown, not emptiness.
//! Unknown layouts, modes, overlays and incomplete dialogs also fail closed.
//! Plain capture drops trailing spaces: a whitespace-only draft may read Empty.
//! Styled input requires the full `capture-pane -p -e -N` viewport. The shared
//! strict decoder rejects unsupported escapes (including conceal), never falling
//! back to Plain. Cursor coordinates are ignored, not used as emptiness proof.
//!
//! Captured: Claude Code 2.1.325, 2026-11-14, U5b styled and P1 styled-N private tmux.
//! An inverse `T` followed by faint non-space suggestion glyphs proves the known
//! placeholder; identical non-faint text is a draft. Blank/padding-only input
//! stays Empty: tmux 3.8/3.6 retained-grid probes show it collides with authored
//! spaces. This is safe ONLY if consumers verify the staged draft exactly before
//! Enter, refusing both false and unknown. Leading spaces are never normalized:
//! ` /clear` is not `/clear` (the former can submit a plain prompt).
//! `staged_draft_matches` supports a single visible row only, Styled only. It
//! refuses multiline/wrapped reconstruction and trailing-padding ambiguity;
//! even a cursor blank is not proof that subsequent spaces are unowned padding.
//!
//! Captured: Claude Code 2.1.325, 2026-11-14, U5b private tmux 100x30 capture-pane -p; sanitized.
//! Header, bordered input and footer are found by chrome, not absolute rows.
//! Status is the transcript tail, independent of blank spacing. Right-aligned
//! footer notices and the deleted-text hint are not turn evidence. Separate
//! update/focus-warning footer rows remain unsupported and return Unknown.
//!
//! Fresh layout evidence does not establish that a particular clear completed.
//! `/clear` remains in history after a successful clear. Consumers own temporal,
//! process/pane identity and copy-mode checks; identical historical text cannot
//! prove that it came from a currently live editor.

use atm_core::styled_screen::StyledScreen;
use atm_core::{
    DialogState, FreshEditorEvidence, InputState, LaunchPolicy, ScreenCursor, ScreenFacts,
    ScreenInput, ScreenTextFormat, TurnState,
};

/// Requires attributes and retained grid spaces (`capture-pane -p -e -N`).
pub const SCREEN_TEXT_FORMAT: ScreenTextFormat = ScreenTextFormat::Styled;

/// Submit only after the caller has established a safe active input.
pub const CLEAR_COMMAND: &str = "/clear";
/// Agent-launch policy, verified by the same capture's launch argv.
pub const LAUNCH_POLICY: LaunchPolicy = LaunchPolicy {
    required_args: &[],
    model_flag: "--model",
};

/// Recognize only complete captured terminal suffixes; unknown never means safe.
#[must_use]
pub fn observe_screen(input: ScreenInput<'_>, cursor: Option<ScreenCursor>) -> ScreenFacts {
    inspect(input, cursor).0
}

fn inspect(input: ScreenInput<'_>, cursor: Option<ScreenCursor>) -> (ScreenFacts, Option<String>) {
    let decoded;
    let (text, styled) = match input {
        ScreenInput::Plain(text) if cursor.is_none() => (text, None),
        ScreenInput::Styled(text) => {
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

/// Compare only a reconstructable active single-line draft, never history.
///
/// Requires Styled. Preserves leading whitespace, refuses multiline/controls and
/// expected trailing spaces. An exact unpadded row can match; trailing-padding
/// differences return None. Consumers must refuse both Some(false) and None.
#[must_use]
pub fn staged_draft_matches(input: ScreenInput<'_>, expected: &str) -> Option<bool> {
    if !matches!(input, ScreenInput::Styled(_))
        || expected.is_empty()
        || expected.ends_with(' ')
        || expected.chars().any(char::is_control)
    {
        return None;
    }
    let (_, draft) = inspect(input, None);
    let draft = draft?;
    // Never remove leading whitespace. A trailing-space-only difference could
    // be authored text or grid padding, so it cannot establish exact equality.
    if draft == expected {
        Some(true)
    } else if draft.trim_end_matches(' ') == expected.trim_end_matches(' ') {
        None
    } else {
        Some(false)
    }
}

fn rule(line: &str, glyph: char) -> bool {
    line.chars().take(8).count() == 8 && line.chars().all(|c| c == glyph)
}

fn header_end(lines: &[&str]) -> Option<usize> {
    let start = lines.iter().position(|line| !line.trim().is_empty())?;
    (lines
        .get(start)
        .and_then(|line| line.strip_prefix(" ▐▛███▛█   Claude Code v"))
        .is_some_and(|version| {
            version
                .split('.')
                .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        })
        && lines
            .get(start + 1)
            .is_some_and(|line| line.starts_with("▝▜██████▀  "))
        && lines
            .get(start + 2)
            .and_then(|line| line.strip_prefix(" ▝▝   ▝▝   "))
            .is_some_and(|cwd| {
                cwd.trim_end() == "~" || cwd.starts_with('/') || cwd.starts_with("~/")
            }))
    .then_some(start + 3)
}

fn dialog(lines: &[&str]) -> Option<DialogState> {
    let top = lines.iter().rposition(|line| rule(line, '─'))?;
    let body: Vec<_> = lines
        .get(top + 1..)?
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    match body.as_slice() {
        ["Accessing workspace:", folder, paragraph @ ..,
            "Claude Code'll be able to read, edit, and execute files here.",
            "Security guide", "❯ No, exit", "Yes, I trust this folder",
            "Enter to confirm · Esc to cancel"]
            if paragraph.join(" ") == "Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team). If not, take a moment to review what's in this folder first."
                && folder.starts_with('/') && !folder.contains(['…', '⤶']) && !folder.contains("...") =>
        {
            Some(DialogState::FolderTrust { folder: (*folder).into() })
        }
        ["Bash command", _description, upper, _command, lower,
            "Ask rule Bash overrides auto mode for this command.",
            "/permissions to let auto mode decide", "Do you want to proceed?",
            "❯ 1. Yes", "2. No", "Esc to cancel · Tab to amend"]
            if rule(upper, '╌') && upper == lower =>
        {
            Some(DialogState::Permission)
        }
        _ => None,
    }
}

fn editor(lines: &[&str], styled: Option<&StyledScreen>) -> Option<(ScreenFacts, Option<String>)> {
    let header_end = header_end(lines);
    let (footer, before_footer) = lines.split_last()?;
    let (footer, before_footer) = if footer.trim() == "Ctrl+Y to paste deleted text" {
        before_footer.split_last()?
    } else {
        (footer, before_footer)
    };
    // The left help chrome is fixed; a separately aligned notice decides no fact.
    let footer = footer
        .trim()
        .split_once("  ")
        .map_or(footer.trim(), |(left, _)| left);
    if !matches!(
        footer,
        "⏵⏵ auto mode on (shift+tab to cycle)"
            | "⏵⏵ auto mode on (shift+tab to cycle) · ← for agents"
    ) {
        return None;
    }
    let lower = before_footer.iter().rposition(|line| rule(line, '─'))?;
    if !before_footer
        .get(lower + 1..)?
        .iter()
        .all(|line| line.trim().is_empty())
    {
        return None;
    }
    let before_lower = lines.get(..lower)?;
    let top = before_lower.iter().rposition(|line| rule(line, '─'))?;
    if lines.get(top) != lines.get(lower) {
        return None;
    }
    let (first, continuation) = lines.get(top + 1..lower)?.split_first()?;
    let input = first.strip_prefix("❯\u{a0}")?;
    let raw_input = styled
        .and_then(|screen| screen.text().lines().nth(top + 1))
        .and_then(|line| line.strip_prefix("❯\u{a0}"));
    let offset = styled.map(|screen| {
        screen
            .text()
            .split_inclusive('\n')
            .take(top + 1)
            .map(str::len)
            .sum::<usize>()
            + "❯\u{a0}".len()
    });
    let draft = (continuation.is_empty())
        .then(|| raw_input.map(str::to_owned))
        .flatten();
    // Preserve any spaces supplied by the caller, though capture-pane -p drops
    // trailing spaces and cannot distinguish a spaces-only draft from empty.
    let input = if !continuation.is_empty() {
        InputState::Draft
    } else if input.is_empty() {
        InputState::Empty
    } else if input.starts_with("Try \"") && input.ends_with('"') {
        match styled.zip(offset) {
            Some((screen, offset))
                if screen.inverse_at(offset) == Some(true)
                    && screen.faint_at(offset) == Some(false)
                    && input
                        .char_indices()
                        .skip(1)
                        .filter(|(_, c)| !c.is_whitespace())
                        .all(|(i, _)| {
                            screen.faint_at(offset + i) == Some(true)
                                && screen.inverse_at(offset + i) == Some(false)
                        }) =>
            {
                InputState::Empty
            }
            Some((screen, offset))
                if input
                    .char_indices()
                    .filter(|(_, c)| !c.is_whitespace())
                    .all(|(i, _)| screen.faint_at(offset + i) == Some(false)) =>
            {
                InputState::Draft
            }
            _ => InputState::Unknown,
        }
    } else {
        InputState::Draft
    };
    // Status follows the transcript, whether the input is inline or bottom-pinned.
    // Only recognized trailing chrome may be skipped, never arbitrary progress text.
    let mut history: Vec<_> = lines
        .get(header_end.unwrap_or(0)..top)?
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    if header_end.is_some() {
        skip_startup_notice(&mut history);
    }
    while history.last().is_some_and(|line| effort_status(line)) {
        history.pop();
    }
    let status = match history.as_slice() {
        [.., spinner, tip] if tip.starts_with("⎿ \u{a0}Tip: ") => Some(*spinner),
        [.., last] => Some(*last),
        [] => None,
    };
    let idle_history = match history.as_slice() {
        [] | ["❯ /clear"] => header_end.is_some(),
        [.., line] => spinner_body(line)
            .is_some_and(|body| body.contains(" for ") && body.contains(" · done ")),
    };
    let turn = if status.is_some_and(running_status) {
        TurnState::Running
    } else if idle_history {
        TurnState::NotRunning
    } else {
        TurnState::Unknown
    };
    let fresh_editor = if header_end.is_some()
        && turn == TurnState::NotRunning
        && input != InputState::Draft
        && matches!(history.as_slice(), [] | ["❯ /clear"])
    {
        FreshEditorEvidence::Present
    } else {
        FreshEditorEvidence::Unknown
    };
    let mut facts = ScreenFacts::default();
    facts.input = input;
    facts.turn = turn;
    facts.dialog = DialogState::None;
    facts.fresh_editor = fresh_editor;
    Some((facts, draft.filter(|_| input == InputState::Draft)))
}

fn skip_startup_notice(history: &mut Vec<&str>) {
    // U5b's one-time auto-mode explanation is chrome, not conversation. Match the
    // whole paragraph after joining wrapped rows, only immediately after the header.
    let Some(end) = history
        .iter()
        .position(|line| *line == "▎ https://code.claude.com/docs/en/permission-modes")
    else {
        return;
    };
    let Some(rows) = history.get(..end) else {
        return;
    };
    let paragraph = rows
        .iter()
        .filter_map(|line| line.strip_prefix("▎ "))
        .collect::<Vec<_>>();
    if paragraph.len() == rows.len() && paragraph.join(" ") == "Auto mode is now Claude Code's default permission mode. Auto mode lets Claude handle permission prompts automatically. Claude checks each tool call for risky actions and prompt injection before executing, runs the ones it assesses as lower-risk, and blocks the rest." {
        history.drain(..=end);
    }
}

fn spinner_body(line: &str) -> Option<&str> {
    let (frame, body) = line.split_once(' ')?;
    (frame.chars().count() == 1 && "·✢*✳✶✻✽".contains(frame)).then_some(body)
}

fn running_status(line: &str) -> bool {
    let Some((verb, details)) = spinner_body(line).and_then(|s| s.split_once('…')) else {
        return false;
    };
    !verb.is_empty()
        && verb.chars().all(char::is_alphabetic)
        && (details.is_empty() || (details.starts_with(" (") && details.ends_with(')')))
}

fn effort_status(line: &str) -> bool {
    let Some((value, notice)) = line.split_once(" · /effort") else {
        return false;
    };
    let Some(level) = value
        .strip_prefix("◐ ")
        .or_else(|| value.strip_prefix("◈ "))
    else {
        return false;
    };
    !level.is_empty()
        && level.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
        && (notice.is_empty() || notice.starts_with(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn observe_screen(text: &str, cursor: Option<ScreenCursor>) -> ScreenFacts {
        super::observe_screen(ScreenInput::Plain(text), cursor)
    }

    macro_rules! facts {
        ($($field:ident: $value:expr),* $(,)?) => {{
            let mut facts = ScreenFacts::default();
            $(facts.$field = $value;)*
            facts
        }};
    }

    macro_rules! capture {
        ($name:literal) => {
            include_str!(concat!(
                "../tests/fixtures/screen_claude-",
                $name,
                "_2.1.325.txt"
            ))
        };
    }

    const IDLE: &str = capture!("after-clear");
    const FRESH: &str = capture!("clear-poll-02");
    const TRUST: &str = capture!("startup");
    const PERMISSION: &str = capture!("permission");
    const PLACEHOLDER: &str = capture!("ready");

    // Captured: Claude Code 2.1.325, 2026-11-14, U5 private-tmux capture; sanitized.
    // README maps every raw capture, including byte-identical duplicates.
    fn cases() -> Vec<(&'static str, &'static str, ScreenFacts)> {
        use DialogState::{None, Permission, Unknown as DialogUnknown};
        use FreshEditorEvidence::{Present, Unknown as NotFresh};
        use InputState::{Draft, Empty, Unknown};
        use TurnState::{NotRunning, Running, Unknown as TurnUnknown};
        let mut cases = Vec::new();
        macro_rules! case {
            ($name:literal, $input:expr, $turn:expr, $dialog:expr, $fresh:expr) => {
                cases.push((
                    $name,
                    capture!($name),
                    facts! {
                        input: $input,
                        turn: $turn,
                        dialog: $dialog,
                        fresh_editor: $fresh,
                    },
                ));
            };
        }
        case!(
            "startup",
            Unknown,
            TurnUnknown,
            DialogState::FolderTrust {
                folder: "/tmp/u8a-fixture/cwd/claude".into(),
            },
            NotFresh
        );
        case!("permission", Unknown, TurnUnknown, Permission, NotFresh);
        case!("denied-0", Unknown, TurnUnknown, Permission, NotFresh);
        case!("before-escape", Empty, Running, None, NotFresh);
        case!("clear-poll-01", Empty, Running, None, NotFresh);
        case!("escape-5", Draft, NotRunning, None, NotFresh);
        case!("clear-poll-02", Empty, NotRunning, None, Present);
        case!("arrival", Empty, NotRunning, None, NotFresh);
        case!("after-clear", Empty, NotRunning, None, NotFresh);
        case!("denied-5", Empty, NotRunning, None, NotFresh);
        case!("gate", Unknown, NotRunning, None, Present);
        case!("ready", Unknown, NotRunning, None, Present);
        case!("restarted", Unknown, NotRunning, None, Present);
        case!("shell", Unknown, TurnUnknown, DialogUnknown, NotFresh);
        case!("final-shell", Unknown, TurnUnknown, DialogUnknown, NotFresh);
        cases
    }

    #[test]
    fn captured_screens() {
        for (name, text, expected) in cases() {
            assert_eq!(observe_screen(text, None), expected, "{name}");
            // RealTmuxClient removes trailing blank rows; recognition must not depend on them.
            assert_eq!(
                observe_screen(text.trim_end_matches('\n'), None),
                expected,
                "{name} trimmed"
            );
        }
    }

    // These fixtures retain their individual Captured: labels; only the body is parser input.
    macro_rules! u5b_capture {
        ($name:literal) => {
            include_str!(concat!(
                "../tests/fixtures/screen_u5b_claude-",
                $name,
                "_2.1.325.txt"
            ))
            .split_once('\n')
            .unwrap()
            .1
        };
    }

    #[test]
    fn u5b_captured_screens() {
        use FreshEditorEvidence::{Present, Unknown as NotFresh};
        use InputState::{Draft, Empty, Unknown};
        let mut cases = Vec::new();
        macro_rules! case {
            ($name:literal, $input:expr, $fresh:expr) => {
                cases.push((
                    $name,
                    u5b_capture!($name),
                    facts! {
                        input: $input,
                        turn: TurnState::NotRunning,
                        dialog: DialogState::None,
                        fresh_editor: $fresh,
                    },
                ));
            };
        }
        // Plain-text suggestions and identical drafts are intentionally indistinguishable.
        case!("poor-fresh", Empty, Present);
        case!("poor-same-pattern-draft", Unknown, Present);
        case!("poor-same-pattern-draft-cursor-start", Unknown, Present);
        case!("poor-different-draft", Draft, NotFresh);
        case!("poor-erased", Empty, Present);
        case!("poor-after-clear", Empty, Present);
        case!("poor-after-clear-same-pattern-draft", Unknown, Present);
        case!("poor-after-clear-different-draft", Draft, NotFresh);
        case!("poor-enabled-fresh", Unknown, Present);
        case!("poor-enabled-same-text-draft", Unknown, Present);
        case!(
            "poor-enabled-same-text-draft-cursor-start",
            Unknown,
            Present
        );
        case!("poor-enabled-different-draft", Draft, NotFresh);
        case!("poor-enabled-erased", Unknown, Present);
        case!("poor-enabled-after-clear", Empty, Present);
        case!("poor-enabled-after-clear-same-text-draft", Unknown, Present);
        case!("poor-enabled-after-clear-different-draft", Draft, NotFresh);
        case!("poor-enabled-after-clear-settled", Empty, Present);
        case!("rich-fresh", Empty, Present);
        case!("rich-same-pattern-draft", Unknown, Present);
        case!("rich-different-draft", Draft, NotFresh);
        case!("rich-erased", Empty, Present);
        case!("rich-after-clear", Empty, Present);
        case!("rich-after-clear-same-pattern-draft", Unknown, Present);
        case!("rich-after-clear-different-draft", Draft, NotFresh);
        case!("rich-enabled-fresh", Unknown, Present);
        case!(
            "rich-enabled-same-text-draft-cursor-start",
            Unknown,
            Present
        );
        case!("rich-enabled-different-draft", Draft, NotFresh);
        case!("rich-enabled-erased", Unknown, Present);
        case!("rich-enabled-after-clear", Empty, Present);
        case!("rich-enabled-after-clear-same-text-draft", Unknown, Present);
        case!("rich-enabled-after-clear-different-draft", Draft, NotFresh);
        case!("rich-enabled-after-clear-settled", Empty, Present);
        case!("rich-enabled-repeat-fresh", Unknown, Present);
        case!("screen-with-colorterm-fresh", Empty, Present);
        case!("screen-with-colorterm-different-draft", Draft, NotFresh);
        case!("screen-with-colorterm-erased", Empty, Present);
        case!("screen-with-colorterm-after-clear", Empty, Present);
        case!(
            "screen-with-colorterm-after-clear-same-pattern-draft",
            Unknown,
            Present
        );
        case!(
            "screen-with-colorterm-after-clear-different-draft",
            Draft,
            NotFresh
        );
        case!("screen-with-colorterm-enabled-fresh", Unknown, Present);
        case!(
            "screen-with-colorterm-enabled-different-draft",
            Draft,
            NotFresh
        );
        case!("screen-with-colorterm-enabled-erased", Unknown, Present);
        case!("screen-with-colorterm-enabled-after-clear", Empty, Present);
        case!(
            "screen-with-colorterm-enabled-after-clear-same-text-draft",
            Unknown,
            Present
        );
        case!(
            "screen-with-colorterm-enabled-after-clear-different-draft",
            Draft,
            NotFresh
        );
        macro_rules! unsupported {
            ($name:literal) => {
                cases.push(($name, u5b_capture!($name), ScreenFacts::default()));
            };
        }
        // Separate update/focus-warning footer rows are not the recognized single-line footer.
        unsupported!("rich-enabled-same-text-draft");
        unsupported!("screen-with-colorterm-same-pattern-draft");
        unsupported!("screen-with-colorterm-enabled-same-text-draft");
        unsupported!("screen-with-colorterm-enabled-same-text-draft-cursor-start");
        assert_eq!(cases.len(), 49);
        for (name, text, expected) in cases {
            assert_eq!(observe_screen(text, None), expected, "{name}");
            assert_eq!(
                observe_screen(text.trim_end_matches('\n'), None),
                expected,
                "{name} trimmed"
            );
        }
    }

    #[test]
    fn footer_notices_and_deleted_text_hint_preserve_facts() {
        // Synthetic: each independent U5b footer feature applied to the U5 idle capture.
        let footer = "⏵⏵ auto mode on (shift+tab to cycle)";
        for suffix in [
            "  ◐ medium · /effort",
            "  ✘ Auto-update failed · Run claude doctor",
            "\n  Ctrl+Y to paste deleted text",
        ] {
            let text = IDLE.replace(footer, &format!("{footer}{suffix}"));
            assert_eq!(
                observe_screen(&text, None),
                facts! {
                    input: InputState::Empty,
                    turn: TurnState::NotRunning,
                    dialog: DialogState::None,
                    fresh_editor: FreshEditorEvidence::Unknown,
                }
            );
        }
        // Synthetic: unseparated prose is not a right-aligned notice; unknown hint is not chrome.
        for suffix in [" novel footer", "\n  Novel dialog"] {
            assert_eq!(
                observe_screen(&IDLE.replace(footer, &format!("{footer}{suffix}")), None),
                ScreenFacts::default()
            );
        }
    }

    #[test]
    fn header_and_composer_are_located_by_chrome() {
        // Synthetic: U5 cleared frame moved vertically, with cwd at HOME and variable borders.
        for blank_rows in [0, 1, 6] {
            for width in [80, 100, 120, 200] {
                let compact = FRESH
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n")
                    .replace("/tmp/u8a-fixture/cwd/claude", "~")
                    .replace(&"─".repeat(120), &"─".repeat(width));
                let text = format!("{}{compact}", "\n".repeat(blank_rows));
                assert_eq!(
                    observe_screen(&text, None),
                    facts! {
                        input: InputState::Empty,
                        turn: TurnState::NotRunning,
                        dialog: DialogState::None,
                        fresh_editor: FreshEditorEvidence::Present,
                    },
                    "rows={blank_rows} width={width}"
                );
            }
        }
    }

    #[test]
    fn idle_done_line_can_immediately_precede_the_input_box() {
        // Synthetic: U5 after-clear's done line two rows above the input's upper border.
        let lines: Vec<_> = IDLE.lines().collect();
        let done = lines
            .iter()
            .position(|line| line.contains(" · done "))
            .unwrap();
        let border = lines
            .iter()
            .position(|line| line.starts_with("────"))
            .unwrap();
        let text = lines[..=done]
            .iter()
            .copied()
            .chain([""])
            .chain(lines[border..].iter().copied())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            observe_screen(&text, None),
            facts! {
                input: InputState::Empty,
                turn: TurnState::NotRunning,
                dialog: DialogState::None,
                fresh_editor: FreshEditorEvidence::Unknown,
            }
        );
    }

    #[test]
    fn only_complete_startup_notice_is_excluded_from_history() {
        // Synthetic: damage the U5b explanatory banner; unknown history cannot prove freshness.
        let text = u5b_capture!("rich-fresh");
        for damaged in [
            text.replace("blocks the rest.", "Unrecognized operation…"),
            text.replace("▎ https://code.claude.com/docs/en/permission-modes", ""),
            format!("unknown transcript\n{text}"),
        ] {
            assert_eq!(
                observe_screen(&damaged, None),
                facts! {
                    input: InputState::Empty,
                    turn: TurnState::Unknown,
                    dialog: DialogState::None,
                    fresh_editor: FreshEditorEvidence::Unknown,
                }
            );
        }
    }

    #[test]
    fn value_variations_preserve_facts() {
        for (name, text, expected) in cases() {
            // Synthetic: value variation of each named U5 fixture; chrome unchanged.
            let varied = text
                .replace("v2.1.325", "v12.34.567")
                .replace(
                    "◐ medium · /effort",
                    "◈ max · /effort · ✘ Auto-update failed · Run claude doctor",
                )
                .replace("✶ Levitating…", "✳ Generating… (2m 31s · ↓ 15.3k tokens)")
                .replace(
                    "✽ Sketching… (running SessionEnd hook · 0s)",
                    "✢ Thinking… (running SessionEnd hook · 14s)",
                )
                .replace("Crunched for 1s", "Calculated for 3m 19s")
                .replace("done 11:25", "done 09:47")
                .replace("Haiku 5.5 · Claude Ultra", "Another-model · Another-plan")
                .replace("/tmp/u8a-fixture/cwd/claude", "/other/project");
            let mut expected = expected;
            if name == "startup" {
                expected.dialog = DialogState::FolderTrust {
                    folder: "/other/project".into(),
                };
            }
            assert_eq!(observe_screen(&varied, None), expected, "{name}");
            // Synthetic: header cwd value variation; trust still requires its full absolute path.
            if name != "startup" {
                assert_eq!(
                    observe_screen(
                        &varied.replace("/other/project", "~/other/project (topic)"),
                        None
                    ),
                    expected,
                    "{name} home cwd"
                );
            }
        }
        // Synthetic: permission's complete current dialog with the historical header scrolled off.
        let cropped = PERMISSION.lines().skip(4).collect::<Vec<_>>().join("\n");
        assert_eq!(
            observe_screen(&cropped, None).dialog,
            DialogState::Permission
        );
        // Synthetic: value variation of before-escape, with optional tip omitted.
        for frame in "·✢*✳✶✻✽".chars() {
            let varied =
                capture!("before-escape").replace("✶ Levitating…", &format!("{frame} Generating…"));
            let varied = varied
                .lines()
                .filter(|line| !line.contains("Tip: "))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(observe_screen(&varied, None).turn, TurnState::Running);
        }
    }

    #[test]
    fn cropped_coordinator_background_footer_is_unknown() {
        // Captured: Claude Code 2.1.325, 2026-11-14, coordinator read-only tmux capture-pane -p; sanitized.
        let text = capture!("extra-running-bottom");
        // Background-agent selection/status beneath the composer is not yet supported.
        assert_eq!(observe_screen(text, None), ScreenFacts::default());
        // Synthetic: bottom-layout variant of extra-running-bottom with the unsupported
        // background footer replaced by the previously captured standard footer.
        let (bottom, _) = text.split_once("  ⏵⏵ auto mode on · 1 shell").unwrap();
        let text = format!("{bottom}  ⏵⏵ auto mode on (shift+tab to cycle)\n");
        assert_eq!(
            observe_screen(&text, None),
            facts! {
                input: InputState::Empty,
                turn: TurnState::Running,
                dialog: DialogState::None,
                fresh_editor: FreshEditorEvidence::Unknown
            }
        );
    }

    #[test]
    fn prose_footer_and_malformed_status_remain_unknown() {
        // Synthetic: structural damage of before-escape and after-clear.
        assert_eq!(
            observe_screen(
                &IDLE.replace("⏵⏵ auto mode on (shift+tab to cycle)", "a prose footer"),
                None
            ),
            ScreenFacts::default()
        );
        for status in [
            "Generating…",
            "? Generating…",
            "✻ Generating",
            "✻ Generating… (unterminated",
        ] {
            assert_eq!(
                observe_screen(
                    &capture!("before-escape").replace("✶ Levitating…", status),
                    None
                )
                .turn,
                TurnState::Unknown
            );
        }
    }

    #[test]
    fn control_policy_uses_captured_commands() {
        assert_eq!(CLEAR_COMMAND, "/clear");
        assert!(LAUNCH_POLICY.required_args.is_empty());
        assert_eq!(LAUNCH_POLICY.model_flag, "--model");
    }

    #[test]
    fn truncated_editor_is_unknown() {
        // Synthetic: remove required frame regions rather than guessing partial UI semantics.
        let lines: Vec<_> = IDLE.lines().collect();
        for text in [
            lines[36..].join("\n"),
            lines[..38].join("\n"),
            "❯\u{a0}".into(),
        ] {
            assert_eq!(observe_screen(&text, None), ScreenFacts::default());
        }
        let border = "─".repeat(120);
        assert_eq!(
            observe_screen(&IDLE.replacen(&border, "", 1), None),
            ScreenFacts::default()
        );
    }

    #[test]
    fn trust_paragraph_wraps_do_not_change_the_dialog() {
        // Synthetic: re-wrap the real U5 trust paragraph at 80, 100, 120 and 200 columns.
        let start = TRUST.find(" Quick safety check:").unwrap();
        let end = TRUST.find(" Claude Code'll").unwrap();
        let paragraph = TRUST[start..end]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for width in [80, 100, 120, 200] {
            let mut wrapped = String::from(" ");
            let mut column = 1;
            for word in paragraph.split(' ') {
                if column > 1 && column + word.len() + 1 > width {
                    wrapped.push_str("\n ");
                    column = 1;
                } else if column > 1 {
                    wrapped.push(' ');
                    column += 1;
                }
                wrapped.push_str(word);
                column += word.len();
            }
            let text = format!("{}{}\n\n{}", &TRUST[..start], wrapped, &TRUST[end..]);
            assert_eq!(
                observe_screen(&text, None).dialog,
                DialogState::FolderTrust {
                    folder: "/tmp/u8a-fixture/cwd/claude".into(),
                },
                "width={width}"
            );
            assert_eq!(
                observe_screen(
                    &text.replace("Quick safety check:", "Other question:"),
                    None
                ),
                ScreenFacts::default()
            );
        }
    }

    #[test]
    fn clear_echo_below_old_conversation_is_not_idle() {
        // Synthetic: U5 clear-poll-01 before its SessionEnd hook spinner appears.
        let text = capture!("clear-poll-01")
            .lines()
            .filter(|line| !line.contains("Sketching…") && !line.contains("Tip: "))
            .collect::<Vec<_>>()
            .join("\n");
        let facts = observe_screen(&text, None);
        assert_eq!(facts.input, InputState::Empty);
        assert_eq!(facts.turn, TurnState::Unknown);
        assert_eq!(facts.fresh_editor, FreshEditorEvidence::Unknown);
        assert_eq!(observe_screen(FRESH, None).turn, TurnState::NotRunning);
    }

    #[test]
    fn wrapped_or_truncated_trust_path_is_not_trusted() {
        // Synthetic: an incomplete displayed path must never become allowlist evidence.
        for path in [
            "/tmp/u8a-fixture/…",
            "/tmp/u8a-fixture/...",
            "/tmp/u8a-\nfixture/cwd/claude",
            "relative/path",
            "",
        ] {
            let text = TRUST.replace("/tmp/u8a-fixture/cwd/claude", path);
            assert_eq!(
                observe_screen(&text, None),
                ScreenFacts::default(),
                "{path:?}"
            );
        }
        let text = TRUST.replace(" Enter to confirm · Esc to cancel", "");
        assert_eq!(observe_screen(&text, None), ScreenFacts::default());
    }

    #[test]
    fn unexpected_trust_selection_is_not_default_trust() {
        // Synthetic: Down/Enter is only established for the captured default-No selection.
        let text = TRUST
            .replace("❯ No, exit", "  No, exit")
            .replace("  Yes, I trust this folder", "❯ Yes, I trust this folder");
        assert_eq!(observe_screen(&text, None), ScreenFacts::default());
    }

    #[test]
    fn history_does_not_override_active_composer() {
        assert_eq!(
            observe_screen(capture!("escape-5"), None).input,
            InputState::Draft
        );
        assert_eq!(observe_screen(FRESH, None).input, InputState::Empty);
        // Synthetic: permission/working vocabulary in earlier transcript is not active UI.
        let text = IDLE.replace(
            "● ok",
            "● Do you want to proceed?\n✶ Levitating…\nEnter to confirm · Esc to cancel",
        );
        assert_eq!(observe_screen(&text, None), observe_screen(IDLE, None));
    }

    #[test]
    fn placeholder_collision_never_authorizes_paste() {
        assert_eq!(observe_screen(PLACEHOLDER, None).input, InputState::Unknown);
        // Synthetic: identical draft text has the identical plain-text representation.
        let draft = IDLE.replace("❯\u{a0}\n", "❯\u{a0}Try \"how do I log an error?\"\n");
        assert_eq!(observe_screen(&draft, None).input, InputState::Unknown);
    }

    #[test]
    fn whitespace_and_multiline_drafts_are_not_empty() {
        // Synthetic: captured empty composer with significant user input inserted.
        // ASCII trailing-space cases check parser preservation only: capture-pane -p
        // drops those spaces, so they do NOT prove end-to-end spaces-only draft safety.
        for draft in [
            " ",
            "  ",
            "\u{a0}",
            "hello\nworld",
            "\n",
            "\n ",
            "日本語🦀",
            "\u{200b}",
        ] {
            let text = IDLE.replace("❯\u{a0}\n", &format!("❯\u{a0}{draft}\n"));
            assert_eq!(
                observe_screen(&text, None).input,
                InputState::Draft,
                "{draft:?}"
            );
            assert_eq!(
                observe_screen(&text, None).fresh_editor,
                FreshEditorEvidence::Unknown
            );
        }
    }

    #[test]
    fn unknown_status_or_suffix_never_proves_idle() {
        // Synthetic: unfamiliar active status, footer or appended shell/overlay.
        let text = capture!("before-escape").replace("✶ Levitating…", "⟳ Unrecognized activity");
        assert_eq!(observe_screen(&text, None).turn, TurnState::Unknown);
        for text in [
            format!("{IDLE}shell$ "),
            format!("{IDLE}Unknown dialog"),
            IDLE.replace("auto mode on", "unrecognized mode"),
        ] {
            assert_eq!(observe_screen(&text, None), ScreenFacts::default());
        }
    }

    #[test]
    fn status_is_located_by_chrome_not_distance_from_input() {
        // Synthetic: vertical spacing doesn't hide a recognized spinner or imply idle.
        let border = "─".repeat(120);
        let shifted = capture!("before-escape").replacen(&border, &format!("\n\n\n\n{border}"), 1);
        assert_eq!(observe_screen(&shifted, None).turn, TurnState::Running);
        let unfamiliar = IDLE.replace(
            "✻ Crunched for 1s · done 11:25",
            "Unrecognized active operation",
        );
        assert_eq!(observe_screen(&unfamiliar, None).turn, TurnState::Unknown);
    }

    #[test]
    fn orphan_tip_does_not_prove_idle() {
        // Synthetic: structural damage of clear-poll-01 removes the spinner but leaves its tip.
        let text =
            capture!("clear-poll-01").replace("✽ Sketching… (running SessionEnd hook · 0s)", "");
        let facts = observe_screen(&text, None);
        assert_eq!(facts.turn, TurnState::Unknown);
        assert_eq!(facts.fresh_editor, FreshEditorEvidence::Unknown);
    }

    #[test]
    fn permission_requires_complete_dialog() {
        // Synthetic: don't infer permission from only Yes/No or a missing command box.
        for text in [
            PERMISSION.replace("Do you want to proceed?", ""),
            PERMISSION.replace("Bash command", "Novel dialog"),
            PERMISSION.replace("Esc to cancel · Tab to amend", ""),
        ] {
            assert_eq!(observe_screen(&text, None), ScreenFacts::default());
        }
    }

    #[test]
    fn controls_and_unverified_cursor_are_unknown() {
        // Synthetic: no U5 screen contained these controls or cursor metadata.
        for control in ['\x1b', '\r', '\t', '\0', '\x7f'] {
            let text = IDLE.replace("❯\u{a0}", &format!("❯\u{a0}{control}"));
            assert_eq!(observe_screen(&text, None), ScreenFacts::default());
        }
        for cursor in [
            ScreenCursor { column: 2, row: 36 },
            ScreenCursor {
                column: u32::MAX,
                row: u32::MAX,
            },
        ] {
            assert_eq!(observe_screen(IDLE, Some(cursor)), ScreenFacts::default());
            assert_eq!(
                observe_screen(PLACEHOLDER, Some(cursor)),
                ScreenFacts::default()
            );
        }
    }

    #[test]
    fn unknown_and_blank_screens_are_not_empty_editors() {
        for text in [
            "",
            "\n\n",
            " ",
            "❯",
            "Unknown UI",
            "shell$",
            "Yes\nNo\nEsc to cancel",
        ] {
            assert_eq!(observe_screen(text, None), ScreenFacts::default());
        }
    }

    proptest! {
        #[test]
        fn arbitrary_screen_never_panics(
            text in any::<String>(),
            cursor in prop::option::of((any::<u32>(), any::<u32>()))
        ) {
            let cursor = cursor.map(|(column, row)| ScreenCursor { column, row });
            let first = observe_screen(&text, cursor);
            prop_assert_eq!(first, observe_screen(&text, cursor));
        }

        #[test]
        fn fixture_mutations_never_panic(case in 0usize..15, cut in any::<usize>(), inserted in any::<char>()) {
            // Synthetic: truncate real layouts at arbitrary Unicode boundaries, append any scalar.
            let cases = cases();
            let text = cases[case].1;
            let mut chars: Vec<_> = text.chars().collect();
            let cut = cut % (chars.len() + 1);
            chars.truncate(cut);
            chars.push(inserted);
            let text: String = chars.into_iter().collect();
            let _ = observe_screen(&text, None);
        }
    }
}
