//! Conservative screen facts for Codex 0.167.0.
//!
//! Captured: Codex 0.167.0, 2026-11-14, U5 private-tmux capture; sanitized.
//! See `tests/fixtures/README.md` for mappings and missing states. Input is the
//! current `capture-pane -p` viewport, without attributes. Controls except newline,
//! supplied cursor metadata and unsupported/incomplete layouts refuse observation.
//!
//! The placeholder may also be an identical draft: it always remains Unknown.
//! **Consumers cannot deliver into Codex under this plain-text contract yet.**
//! Styled input instead requires `capture-pane -p -e -N`. Coordinates are ignored,
//! never emptiness evidence. The strict decoder refuses unknown/malformed escapes
//! and conceal without fallback. Only the complete regular footer plus the exact
//! all-faint placeholder proves Empty; non-faint text is a draft, including the
//! captured footer without shortcut help. Drafts never grant fresh-editor evidence.
//!
//! Captured: codex-cli 0.167.0, 2026-11-14, U5b styled and P1 styled-N private tmux.
//! `staged_draft_matches` preserves leading whitespace exactly: ` /clear` differs
//! from `/clear`. Consumers must verify the staged draft before Enter (unknown
//! refuses too); padding/whitespace collisions alone are not safe. Tmux 3.8/3.6
//! probes show `-N` retains allocated grid padding, not only authored spaces.
//! Only a single visible row can be compared; multiline/wrapped input returns
//! unknown. Trailing ASCII spaces are chrome: a sealed 0.167.0 probe confirmed
//! `/new   ` executes new, not a prompt. A match does not prove Enter submits:
//! completion popups for `/`, `@` and `$` appear above the composer and must be
//! handled by the consumer, not mistaken for harmless history.
//! Under Plain only the captured `/new` completion is a known draft; free-text
//! drafts lack shortcut chrome and remain Unknown. P1 permission/trust dialogs
//! remain unsupported; no speculative action/selection recognizer is added.
//!
//! Captured: codex-cli 0.167.0, 2026-11-14, U5b private tmux 100x30 capture-pane -p; sanitized.
//! Fresh means a header and otherwise blank history, with optional startup help
//! and logo. It is not evidence that a particular clear completed: two U5
//! captures from different operations are byte-identical. Consumers own operation
//! transitions, pane/process identity, live viewport, copy-mode and temporal checks.

mod control;
pub use control::{observe_control_screen, validate_hook_list, CONTROL_POLICY};

use atm_core::styled_screen::StyledScreen;
use atm_core::{
    DialogState, FreshEditorEvidence, InputState, LaunchPolicy, ScreenCursor, ScreenFacts,
    ScreenInput, ScreenTextFormat, TurnState,
};

#[cfg(test)]
thread_local! {
    static DECODE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Requires attributes and retained grid spaces (`capture-pane -p -e -N`).
pub const SCREEN_TEXT_FORMAT: ScreenTextFormat = ScreenTextFormat::Styled;

/// Submit only after the caller has established a safe active input.
pub const CLEAR_COMMAND: &str = "/new";
/// Agent launches must preserve these args, independently of legacy spawn defaults.
pub const LAUNCH_POLICY: LaunchPolicy = LaunchPolicy {
    required_args: &["--no-daemon"],
    model_flag: "-m",
};

const HEADER_PREFIX: &str = "  >_ OpenAI Codex (v";
const REVIEW_HELP: &str = "/review - review any changes and find issues";
const STARTUP_HELP: &[&str] = &[
    "To get started, describe a task or try one of these commands:",
    "/init - create an AGENTS.md file with instructions for Codex",
    "/status - show current session configuration",
    "/permissions - choose what Codex is allowed to do",
    "/model - choose what model and reasoning effort to use",
    REVIEW_HELP,
];
const LOGO: &[&str] = &[
    "⣀⣤⣤⣤⣀⣀",
    "⣠⣶⣿⣿⣿⣿⣿⣿⣿⣿⣦⣤⣤⣤⣤⣤⣤⡀",
    "⢀⣾⣿⣿⠿⠋⠉⠉⢉⣭⣿⣿⣿⣿⣿⠿⣿⣿⣿⣿⣷⣄",
    "⣀⣾⣿⣿⠃ ⣀⣴⣾⣿⣿⡿⠟⠋ ⣀⡀ ⠈⠙⢿⣿⣿⣧",
    "⣀⣶⣿⣿⣿⣿⡇ ⢸⣿⣿⡿⠛⠉⢀⣠⣴⣿⣿⣿⣷⣦⣀⠈⢻⣿⣿⡇",
    "⣰⣿⣿⡿⢿⣿⣿⡇ ⢸⣿⣿⢀⣤⣶⣿⣿⣿⠟⠛⠻⣿⣿⣿⣿⣮⣿⣿⡷",
    "⢰⣿⣿⡟⠁⢸⣿⣿⡇ ⢸⣿⣿⣿⣿⠿⠿⣿⣿⣿⣦⣄⡀⠈⠛⠿⣿⣿⣿⣧⡀",
    "⣾⣿⣿⠃ ⢸⣿⣿⡇ ⢸⣿⣿⠋⠁  ⠈⠙⢻⣿⣿⣿⣶⣤⡀ ⠹⢿⣿⣷⡄",
    "⢸⣿⣿⣇ ⠸⣿⣿⣷⣦⣼⣿⣿      ⢸⣿⣿⠻⢿⣿⣿⡇ ⠘⣿⣿⣿",
    "⠈⢿⣿⣿⣦⡀⠈⠛⠿⣿⣿⣿⣿⣄⡀  ⢀⣠⣼⣿⣿  ⣿⣿⡇  ⣿⣿⣿⠇",
    "⢻⣿⣿⣿⣷⣦⣀⠈⠙⠻⣿⣿⣿⣶⣶⣿⣿⣿⣿⣿  ⣿⣿⡇ ⣠⣿⣿⣿",
    "⢸⣿⣿⡿⢿⣿⣿⣿⣦⣠⣴⣿⣿⣿⡿⠟⠋⢸⣿⣿  ⣿⣿⣷⣴⣿⣿⣿⠃",
    "⠘⣿⣿⣷ ⠈⠛⢿⣿⣿⣿⠿⠋⠉ ⣠⣴⣿⣿⣿  ⣿⣿⣿⣿⣿⠟⠁",
    "⠻⣿⣿⣿⣤⣀ ⠈⠉ ⢀⣤⣶⣿⣿⣿⠿⠟⠁ ⣰⣿⣿⡟⠋⠁",
    "⠈⠿⣿⣿⣿⣿⣶⣾⣿⣿⣿⣿⠟⠋⠁  ⣠⣼⣿⣿⡟",
    "⠙⠛⠛⠛⠻⠛⢿⣿⣿⣿⣿⣷⣾⣿⣿⣿⡿⠏",
    "⠈⠙⠛⠿⠿⠿⠿⠛⠉",
];

/// Recognize complete captured suffixes; unknown never authorizes input.
#[must_use]
pub fn observe_screen(input: ScreenInput<'_>, cursor: Option<ScreenCursor>) -> ScreenFacts {
    inspect(input, cursor).0
}

fn inspect(input: ScreenInput<'_>, cursor: Option<ScreenCursor>) -> (ScreenFacts, Option<String>) {
    let decoded;
    let (text, styled) = match input {
        ScreenInput::Plain(text) if cursor.is_none() => (text, None),
        ScreenInput::Styled(text) => {
            #[cfg(test)]
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
    inspect_prepared(text, styled)
}

fn inspect_prepared(text: &str, styled: Option<&StyledScreen>) -> (ScreenFacts, Option<String>) {
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
    if known_dialog(&lines) {
        let mut facts = ScreenFacts::default();
        facts.dialog = DialogState::Other;
        return (facts, None);
    }
    editor(&lines, styled).unwrap_or_default()
}

/// Compare only a reconstructable active single-line draft, never history.
///
/// Requires Styled and a nonempty expected string without controls or trailing
/// whitespace. Compares after removing trailing ASCII spaces from the draft only;
/// leading whitespace stays exact. Other differences return Some(false); unknown
/// layouts, faint text and multiline/wrapped drafts return None. A match proves
/// visible text only, not that Enter submits it: consumers must handle completion
/// popups (including `/`, `@` and `$`) and refuse both Some(false) and None.
#[must_use]
pub fn staged_draft_matches(input: ScreenInput<'_>, expected: &str) -> Option<bool> {
    if !matches!(input, ScreenInput::Styled(_))
        || expected.is_empty()
        || expected.ends_with(char::is_whitespace)
        || expected.chars().any(char::is_control)
    {
        return None;
    }
    let (_, draft) = inspect(input, None);
    draft_matches(draft.as_deref(), expected)
}

fn draft_matches(draft: Option<&str>, expected: &str) -> Option<bool> {
    Some(draft?.trim_end_matches(' ') == expected)
}

fn nonblank<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect()
}

fn header<'a>(lines: &[&'a str]) -> Option<(usize, &'a str)> {
    let index = lines.iter().position(|line| {
        line.strip_prefix(HEADER_PREFIX)
            .and_then(|s| s.strip_suffix(')'))
            .is_some_and(version)
    })?;
    let cwd = lines.get(index + 1)?.strip_prefix("     ")?.trim_end();
    (cwd == "~" || cwd.starts_with('/') || cwd.starts_with("~/")).then_some((index, cwd))
}

fn known_dialog(lines: &[&str]) -> bool {
    let body = nonblank(lines);
    if update_prompt(&body) {
        return true;
    }
    matches!(body.as_slice(),
        [.., title, message, "k keep & next · esc dismiss & close · ^o copy · ←/→ warning"]
            if !message.is_empty()
                && title
                    .strip_prefix("Warnings · ")
                    .and_then(|s| s.strip_suffix(" · Startup"))
                    .and_then(|s| s.split_once(" of "))
                    .is_some_and(|(index, total)| {
                        positive_integer(index) && positive_integer(total)
                    })
    )
}

fn update_prompt(body: &[&str]) -> bool {
    matches!(body,
        [update, "Release notes: https://github.com/openai/codex/releases/latest", "› 1. Update now (runs `npm install -g @openai/codex`)", "2. Skip", "3. Skip until next version", "enter continue · esc skip"]
            if update
                .strip_prefix("Update available · ")
                .and_then(|s| s.split_once(" → "))
                .is_some_and(|(old, new)| version(old) && version(new))
    )
}

fn editor(lines: &[&str], styled: Option<&StyledScreen>) -> Option<(ScreenFacts, Option<String>)> {
    let header_index = header(lines).map(|(index, _)| index);
    let (last, before_last) = lines.split_last()?;
    let shortcuts = last.split_whitespace().collect::<Vec<_>>().join(" ");
    let regular = shortcuts == "← for agents · ? for shortcuts"
        || shortcuts
            .strip_prefix("← for agents · ? for shortcuts ⚠ ")
            .and_then(|s| s.strip_suffix(" · f2 to view"))
            .and_then(|s| s.split_once(' '))
            .is_some_and(|(count, label)| {
                positive_integer(count) && matches!(label, "warning" | "warnings")
            });
    // A typed composer drops shortcut help but may retain warning chrome.
    let warning_only = styled.is_some()
        && shortcuts
            .strip_prefix("⚠ ")
            .and_then(|s| s.strip_suffix(" · f2 to view"))
            .and_then(|s| s.split_once(' '))
            .is_some_and(|(count, label)| {
                positive_integer(count) && matches!(label, "warning" | "warnings")
            });
    let (model, before_model) = if regular || warning_only {
        before_last.split_last()?
    } else {
        (last, before_last)
    };
    let (model, location) = model.strip_prefix("  ")?.trim_end().split_once(" · ")?;
    let (name, effort) = model.rsplit_once(' ')?;
    if name.is_empty()
        || effort.is_empty()
        || !effort.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
        || !(location == "~" || location.starts_with('/') || location.starts_with("~/"))
    {
        return None;
    }
    let (separator, before_separator) = before_model.split_last()?;
    let (composer, before_composer) = before_separator.split_last()?;
    if !separator.is_empty() {
        return None;
    }
    let typed = composer.strip_prefix("› ");
    let offset = styled.map(|screen| {
        screen
            .text()
            .split_inclusive('\n')
            .take(before_composer.len())
            .map(str::len)
            .sum::<usize>()
            + "› ".len()
    });
    let normal_draft = styled
        .zip(offset)
        .zip(typed)
        .is_some_and(|((screen, offset), text)| {
            !text.is_empty()
                && text
                    .char_indices()
                    .all(|(i, _)| screen.faint_at(offset + i) == Some(false))
        });
    let (input, dialog, history) = if regular && *composer == "› Ask Codex to do anything" {
        let input = match styled.zip(offset).zip(typed) {
            Some(((screen, offset), text))
                if text.char_indices().all(|(i, _)| {
                    screen.faint_at(offset + i) == Some(true)
                        && screen.inverse_at(offset + i) == Some(false)
                }) =>
            {
                InputState::Empty
            }
            _ if normal_draft => InputState::Draft,
            _ => InputState::Unknown,
        };
        (input, DialogState::None, before_composer)
    } else if !regular && *composer == "› /new" {
        let (blank, before_blank) = before_composer.split_last()?;
        let (menu, history) = before_blank.split_last()?;
        if !blank.is_empty() || *menu != "› /new  start a new chat during a conversation" {
            return None;
        }
        (InputState::Draft, DialogState::Other, history)
    } else if normal_draft {
        (InputState::Draft, DialogState::None, before_composer)
    } else {
        return None;
    };
    let body = nonblank(history.get(header_index.map_or(0, |index| index + 2)..)?);
    let fresh_body = body.strip_prefix(STARTUP_HELP).unwrap_or(&body);
    let fresh = header_index.is_some_and(|index| {
        history
            .get(..index)
            .is_some_and(|prefix| prefix.iter().all(|line| line.trim().is_empty()))
    }) && (fresh_body.is_empty() || fresh_body == LOGO)
        && dialog == DialogState::None;
    let last_history = body.last().copied().unwrap_or_default();
    let turn = if last_history
        .strip_prefix("• Working (")
        .and_then(|s| s.strip_suffix(" • esc to interrupt)"))
        .is_some_and(duration)
    {
        TurnState::Running
    } else if fresh
        || worked_status(last_history)
        || last_history == "■ Conversation interrupted - use /feedback if something went wrong"
        || last_history == REVIEW_HELP
        || status_output(&body)
    {
        TurnState::NotRunning
    } else {
        TurnState::Unknown
    };
    let mut facts = ScreenFacts::default();
    facts.input = input;
    facts.turn = turn;
    facts.dialog = dialog;
    if fresh && input != InputState::Draft {
        facts.fresh_editor = FreshEditorEvidence::Present;
    }
    let draft = styled
        .filter(|_| input == InputState::Draft && normal_draft)
        .and_then(|screen| screen.text().lines().nth(before_composer.len()))
        .and_then(|line| line.strip_prefix("› "))
        .map(str::to_owned);
    Some((facts, draft))
}

fn positive_integer(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|c| c.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|n| n > 0)
}

fn version(value: &str) -> bool {
    value.split('.').count() >= 2
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

fn duration(value: &str) -> bool {
    !value.is_empty()
        && value.split(' ').all(|part| {
            part.strip_suffix(['h', 'm', 's']).is_some_and(|number| {
                !number.is_empty() && number.chars().all(|c| c.is_ascii_digit())
            })
        })
}

fn worked_duration(value: &str) -> bool {
    let component = |part: &str, suffix: char, range: std::ops::RangeInclusive<u64>| {
        part.strip_suffix(suffix)
            .filter(|n| *n == "0" || (!n.starts_with('0') && positive_integer(n)))
            .and_then(|n| n.parse::<u64>().ok())
            .is_some_and(|n| range.contains(&n))
    };
    match value.split(' ').collect::<Vec<_>>().as_slice() {
        ["<1s"] => true,
        [seconds] => component(seconds, 's', 1..=59),
        [minutes, seconds] => component(minutes, 'm', 1..=59) && component(seconds, 's', 0..=59),
        [hours, minutes, seconds] => {
            component(hours, 'h', 1..=u64::MAX)
                && component(minutes, 'm', 0..=59)
                && component(seconds, 's', 0..=59)
        }
        _ => false,
    }
}

fn worked_status(line: &str) -> bool {
    let Some((elapsed, clock)) = line
        .strip_prefix("Worked for ")
        .and_then(|s| s.split_once(" • "))
    else {
        return false;
    };
    let (clock, hours) = match clock.split_once(' ') {
        Some((clock, "AM" | "PM")) if !clock.starts_with('0') => (clock, 1..=12),
        None if clock.len() == 5 => (clock, 0..=23),
        _ => return false,
    };
    // Finished durations are unpadded and range-bounded, unlike running status.
    worked_duration(elapsed)
        && clock.split_once(':').is_some_and(|(hour, minute)| {
            (1..=2).contains(&hour.len())
                && hour.chars().all(|c| c.is_ascii_digit())
                && hour.parse::<u8>().is_ok_and(|n| hours.contains(&n))
                && minute.len() == 2
                && minute.chars().all(|c| c.is_ascii_digit())
                && minute.parse::<u8>().is_ok_and(|n| n < 60)
        })
}

fn status_output(body: &[&str]) -> bool {
    // Require the complete status field sequence; a mention of permissions is not a modal.
    let fields = [
        "Model:",
        "Model provider:",
        "Directory:",
        "Permissions:",
        "Agents.md:",
        "Account:",
        "ChatGPT workspace:",
        "Collaboration mode:",
        "Session:",
        "Weekly limit:",
        "Credits:",
    ];
    body.get(body.len().saturating_sub(fields.len())..)
        .is_some_and(|tail| {
            tail.len() == fields.len()
                && tail.iter().zip(fields).all(|(line, label)| {
                    line.strip_prefix(label)
                        .is_some_and(|value| !value.trim().is_empty())
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn captured_minute_completion_line_is_recognized() {
        // Captured: codex-cli 0.167.0, 2026-11-14, sealed loopback stub held
        // 65 seconds, private tmux capture-pane -p -e -N at both widths.
        for capture in [
            include_str!("../tests/fixtures/screen_styled_u8g_finished-100x32_0.167.0.txt"),
            include_str!("../tests/fixtures/screen_styled_u8g_finished-120x40_0.167.0.txt"),
        ] {
            let text = capture.split_once('\n').unwrap().1;
            let decoded = StyledScreen::parse(text).unwrap();
            let line = decoded
                .text()
                .lines()
                .map(str::trim)
                .find(|line| line.starts_with("Worked for "))
                .unwrap();
            assert!(worked_status(line), "{line}");
            // Launched with --no-alt-screen, -C <dir> and a positional prompt.
            // P3 isolated --no-alt-screen as the compact-footer trigger, not -C
            // or the prompt; prompt-started Codex via today's atm spawn has the
            // normal footer. Duration evidence must not broaden the editor gate.
            assert_eq!(
                super::observe_screen(ScreenInput::Styled(text), None).turn,
                TurnState::Unknown
            );
        }
    }

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
                "../tests/fixtures/screen_codex-",
                $name,
                "_0.167.0.txt"
            ))
        };
    }
    const IDLE: &str = capture!("after-clear");
    const FRESH: &str = capture!("clear-poll-01");
    const UPDATE: &str = capture!("startup");
    const WARNING: &str = capture!("warning");
    const RUNNING: &str = capture!("before-escape");
    const COMPLETION: &str = capture!("clear-poll-00");

    // Captured: Codex 0.167.0, 2026-11-14, U5 private-tmux capture; sanitized.
    fn cases() -> Vec<(&'static str, &'static str, ScreenFacts)> {
        use DialogState::{None, Other, Unknown as DialogUnknown};
        use FreshEditorEvidence::{Present, Unknown as NotFresh};
        use InputState::{Draft, Unknown};
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
        case!("startup", Unknown, TurnUnknown, Other, NotFresh);
        case!("restart-startup", Unknown, TurnUnknown, Other, NotFresh);
        case!("warning", Unknown, TurnUnknown, Other, NotFresh);
        case!("post-update", Unknown, NotRunning, None, NotFresh);
        case!("status", Unknown, NotRunning, None, NotFresh);
        case!("arrival", Unknown, NotRunning, None, NotFresh);
        case!("after-clear", Unknown, NotRunning, None, NotFresh);
        case!("permission", Unknown, NotRunning, None, NotFresh);
        case!("escape-5", Unknown, NotRunning, None, NotFresh);
        case!("before-escape", Unknown, Running, None, NotFresh);
        case!("clear-poll-00", Draft, NotRunning, Other, NotFresh);
        case!("clear-poll-01", Unknown, NotRunning, None, Present);
        case!("shell", Unknown, TurnUnknown, DialogUnknown, NotFresh);
        case!("final-shell", Unknown, TurnUnknown, DialogUnknown, NotFresh);
        cases
    }

    #[test]
    fn captured_screens() {
        for (name, text, expected) in cases() {
            assert_eq!(observe_screen(text, None), expected, "{name}");
            assert_eq!(
                observe_screen(text.trim_end_matches('\n'), None),
                expected,
                "{name} trimmed"
            );
        }
    }

    #[test]
    fn u5b_captured_screens() {
        let fresh = facts! {
            input: InputState::Unknown,
            turn: TurnState::NotRunning,
            dialog: DialogState::None,
            fresh_editor: FreshEditorEvidence::Present,
        };
        let mut cases = Vec::new();
        macro_rules! case {
            ($mode:literal, $state:literal, $want:expr) => {
                cases.push((
                    concat!($mode, "-", $state),
                    include_str!(concat!(
                        "../tests/fixtures/screen_u5b_codex-",
                        $mode,
                        "-",
                        $state,
                        "_0.167.0.txt"
                    ))
                    .split_once('\n')
                    .unwrap()
                    .1,
                    $want,
                ));
            };
        }
        macro_rules! mode {
            ($mode:literal) => {
                case!($mode, "fresh", fresh.clone());
                case!($mode, "erased", fresh.clone());
                case!($mode, "after-clear", fresh.clone());
                case!($mode, "same-text-draft", ScreenFacts::default());
                case!(
                    $mode,
                    "same-text-draft-cursor-start",
                    ScreenFacts::default()
                );
                case!($mode, "different-draft", ScreenFacts::default());
                case!($mode, "after-clear-same-text-draft", ScreenFacts::default());
                case!($mode, "after-clear-different-draft", ScreenFacts::default());
            };
        }
        // Captured: codex-cli 0.167.0, 2026-11-14, U5b private tmux 100x30 capture-pane -p; sanitized.
        mode!("rich");
        mode!("poor");
        mode!("screen-with-colorterm");
        assert_eq!(cases.len(), 24);
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
    fn fresh_header_and_running_status_have_no_absolute_rows() {
        // Synthetic: relocate U5 header/status by changing only blank spacing and cwd.
        for blanks in [0, 1, 6] {
            let text = format!("{}{}", "\n".repeat(blanks), FRESH.trim_start_matches('\n'))
                .replace("/tmp/u8a-fixture/cwd/codex", "~");
            assert_eq!(
                observe_screen(&text, None).fresh_editor,
                FreshEditorEvidence::Present
            );
        }
        let text = RUNNING.replace(
            "• Working (0s • esc to interrupt)",
            "• Working (0s • esc to interrupt)\n\n\n\n",
        );
        assert_eq!(observe_screen(&text, None).turn, TurnState::Running);
    }

    #[test]
    fn value_variations_preserve_facts() {
        for (name, text, expected) in cases() {
            // Synthetic: value variation of each named U5 fixture; no new real Codex capture.
            let varied = text
                .replace("0.167.0", "12.34.567")
                .replace("0.167.1", "12.35.0")
                .replace("0.169.0", "13.0.0")
                .replace("GPT-6-Luna low", "Different-Model xhigh")
                .replace(
                    "/tmp/u8a-fixture/cwd/codex",
                    "~/another/project (feature/topic)",
                )
                .replace("Reply ok", "A different conversation title")
                .replace("Worked for 1s • 11:31 AM", "Worked for 12m 3s • 9:45 PM")
                .replace(
                    "Worked for 2s • 11:32 AM",
                    "Worked for 1h 2m 30s • 10:59 AM",
                )
                .replace("Working (0s •", "Working (4m 15s •")
                .replace("⚠ 1 warning", "⚠ 12 warnings")
                .replace(
                    "Warnings · 1 of 1 · Startup",
                    "Warnings · 2 of 12 · Startup",
                )
                .replace("timeout to 3s", "timeout to 17s");
            assert_eq!(observe_screen(&varied, None), expected, "{name}");
        }
        // Synthetic: value/layout variation of after-clear and before-escape: a long
        // conversation scrolls the old header off, but retains the complete active suffix.
        for text in [IDLE, RUNNING] {
            let cropped = text.lines().skip(3).collect::<Vec<_>>().join("\n");
            assert_eq!(observe_screen(&cropped, None), observe_screen(text, None));
        }
    }

    #[test]
    fn malformed_value_shapes_remain_unknown() {
        // Synthetic: structural damage of before-escape's elapsed-status grammar.
        for elapsed in ["", "yesterday", "NaNs", "12qq"] {
            let text = RUNNING.replace("Working (0s •", &format!("Working ({elapsed} •"));
            assert_eq!(observe_screen(&text, None).turn, TurnState::Unknown);
        }
        // Synthetic: malformed update version/choice grammar is not a recognized modal.
        for text in [
            UPDATE.replace("0.167.0", "unknown"),
            UPDATE.replace("3. Skip until next version", "3. Something else"),
        ] {
            assert_eq!(observe_screen(&text, None), ScreenFacts::default());
        }
    }

    #[test]
    fn control_policy_uses_captured_commands() {
        assert_eq!(CLEAR_COMMAND, "/new");
        assert_eq!(LAUNCH_POLICY.required_args, &["--no-daemon"]);
        assert_eq!(LAUNCH_POLICY.model_flag, "-m");
    }

    #[test]
    fn placeholder_collision_never_authorizes_paste() {
        // Synthetic interpretation: a typed identical draft has identical plain text.
        for text in [IDLE, FRESH, RUNNING] {
            assert_eq!(observe_screen(text, None).input, InputState::Unknown);
        }
    }

    #[test]
    fn cursor_at_home_is_not_empty_proof() {
        // Synthetic coordinates: U5 captured no cursor. Home is not proof of a placeholder.
        for cursor in [
            ScreenCursor { column: 2, row: 36 },
            ScreenCursor {
                column: u32::MAX,
                row: u32::MAX,
            },
        ] {
            assert_eq!(observe_screen(FRESH, Some(cursor)), ScreenFacts::default());
        }
    }

    #[test]
    fn old_prompt_arrows_are_not_active_editor() {
        assert_eq!(observe_screen(COMPLETION, None).input, InputState::Draft);
        assert_eq!(observe_screen(COMPLETION, None).dialog, DialogState::Other);
        // Synthetic: transcript arrow and working vocabulary do not override the last composer.
        let text = IDLE.replace("• ok", "› /new\n• Working (0s • esc to interrupt)\n• ok");
        assert_eq!(observe_screen(&text, None).input, InputState::Unknown);
        assert_eq!(observe_screen(&text, None).turn, TurnState::NotRunning);
    }

    #[test]
    fn missing_footer_is_unknown() {
        // Synthetic: missing shortcut, model, header, or active composer; shell/overlay suffix.
        let lines: Vec<_> = IDLE.lines().collect();
        for text in [
            lines[..39].join("\n"),
            lines[..37].join("\n"),
            IDLE.replace("  GPT-6-Luna low ·", "Novel footer without separator"),
            IDLE.replace("› Ask Codex to do anything", ""),
            format!("{IDLE}shell$ "),
            format!("{IDLE}Approve?\nYes\nNo"),
        ] {
            assert_eq!(observe_screen(&text, None), ScreenFacts::default());
        }
    }

    #[test]
    fn update_banner_is_not_update_dialog() {
        assert_eq!(
            observe_screen(capture!("post-update"), None).dialog,
            DialogState::None
        );
        assert_eq!(observe_screen(UPDATE, None).dialog, DialogState::Other);
    }

    #[test]
    fn status_permissions_text_is_not_permission_dialog() {
        for text in [capture!("status"), capture!("permission")] {
            assert_eq!(observe_screen(text, None).dialog, DialogState::None);
            assert_eq!(observe_screen(text, None).turn, TurnState::NotRunning);
        }
    }

    #[test]
    fn fresh_layout_is_deterministic_and_rejects_conversation() {
        // The raw new-picker and clear-poll-01 captures are byte-identical (README).
        // Present is a snapshot fact; calling again cannot prove a transition occurred.
        assert_eq!(
            observe_screen(FRESH, None).fresh_editor,
            FreshEditorEvidence::Present
        );
        assert_eq!(observe_screen(FRESH, None), observe_screen(FRESH, None));
        assert_eq!(
            observe_screen(IDLE, None).fresh_editor,
            FreshEditorEvidence::Unknown
        );
        // Synthetic: crop the logo or leave earlier conversation above it.
        for text in [
            FRESH.replace("⣀⣤⣤⣤⣀⣀", ""),
            FRESH.replacen("\n\n\n", "\n• old conversation\n\n", 1),
        ] {
            assert_eq!(
                observe_screen(&text, None).fresh_editor,
                FreshEditorEvidence::Unknown
            );
        }
    }

    #[test]
    fn incomplete_dialogs_are_unknown() {
        // Synthetic: missing choices/footer or unsupported selection must not recognize a dialog.
        for text in [
            UPDATE.replace("  2. Skip", ""),
            UPDATE.replace("enter continue · esc skip", ""),
            UPDATE.replace("› 1.", "  1."),
            WARNING.replace("Warnings · 1 of 1 · Startup", "Unknown modal"),
            WARNING.replace(
                "k keep & next · esc dismiss & close · ^o copy · ←/→ warning",
                "",
            ),
        ] {
            assert_eq!(observe_screen(&text, None), ScreenFacts::default());
        }
    }

    #[test]
    fn unknown_activity_never_proves_idle() {
        // Synthetic: unsupported or shifted active status must not become idle by absence.
        for text in [
            RUNNING.replace(
                "• Working (0s • esc to interrupt)",
                "• Unrecognized progress",
            ),
            IDLE.replace("Worked for 1s • 11:31 AM", "Unrecognized operation"),
        ] {
            assert_eq!(observe_screen(&text, None).turn, TurnState::Unknown);
        }
    }

    #[test]
    fn unsupported_drafts_controls_and_unknown_screens_are_not_empty() {
        // Synthetic: free-text/newline draft shapes have no real capture; never grant delivery.
        for draft in ["", " ", "日本語🦀", "hello\nworld", "\n", "/new"] {
            let text = IDLE.replace("Ask Codex to do anything", draft);
            assert_ne!(observe_screen(&text, None).input, InputState::Empty);
        }
        for control in ['\x1b', '\r', '\t', '\0', '\x7f'] {
            assert_eq!(
                observe_screen(&format!("{IDLE}{control}"), None),
                ScreenFacts::default()
            );
        }
        for text in ["", "\n\n", " ", "Unknown UI", "Yes\nNo\nEsc to cancel"] {
            assert_eq!(observe_screen(text, None), ScreenFacts::default());
        }
    }

    proptest! {
        #[test]
        fn arbitrary_screen_never_panics(text in any::<String>(), cursor in prop::option::of((any::<u32>(), any::<u32>()))) {
            let cursor = cursor.map(|(column, row)| ScreenCursor { column, row });
            prop_assert_eq!(observe_screen(&text, cursor), observe_screen(&text, cursor));
        }
        #[test]
        fn fixture_mutations_never_panic(case in 0usize..14, cut in any::<usize>(), inserted in any::<char>()) {
            // Synthetic: truncate captured layouts at Unicode boundaries, append any scalar.
            let cases = cases();
            let mut chars: Vec<_> = cases[case].1.chars().collect();
            chars.truncate(cut % (chars.len() + 1));
            chars.push(inserted);
            let _ = observe_screen(&chars.into_iter().collect::<String>(), None);
        }
    }
}
