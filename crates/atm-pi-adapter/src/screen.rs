//! Conservative screen facts for pi 1.5.0.
//!
//! Captured: pi 1.5.0, 2026-11-14, U5 private-tmux capture; sanitized.
//! See `tests/fixtures/README.md` for source mappings and missing states.
//! Input is a current `capture-pane -p` viewport (no attributes). Controls other
//! than newline, unverified cursor metadata, unknown footers/statuses and partial
//! frames refuse observation. Trailing blank rows may be omitted.
//!
//! Pi has no native approval dialogs; unknown extension UI remains Unknown.
//! Free-text drafts have only synthetic safety coverage, not real captures.
//! The active upper border, not startup's interrupt help, establishes turn state.
//! Fresh layout requires zero usage and startup-only history: the new-session
//! notice survives later turns and alone proves nothing. Present does not prove
//! a particular clear completed. Consumers own viewport freshness, copy-mode,
//! process/pane identity and temporal delivery checks.

use atm_core::{
    DialogState, FreshEditorEvidence, InputState, LaunchPolicy, ScreenCursor, ScreenFacts,
    TurnState,
};

/// Submit only after the caller has established a safe active input.
pub const CLEAR_COMMAND: &str = "/new";
/// Agent-launch policy verified by the capture's launch argv.
pub const LAUNCH_POLICY: LaunchPolicy = LaunchPolicy {
    required_args: &[],
    model_flag: "--model",
};

/// Recognize complete captured layouts only; unknown never authorizes input.
#[must_use]
pub fn observe_screen(text: &str, cursor: Option<ScreenCursor>) -> ScreenFacts {
    if cursor.is_some() || text.chars().any(|c| c.is_control() && c != '\n') {
        return ScreenFacts::default();
    }
    let mut lines: Vec<_> = text.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    editor(&lines).unwrap_or_default()
}

fn rule(line: &str) -> bool {
    line.chars().count() >= 8 && line.chars().all(|c| c == '─')
}

fn editor(lines: &[&str]) -> Option<ScreenFacts> {
    if lines.get(1) != Some(&" ▀▀█  v1.5.0")
        || lines.get(2) != Some(&" █▀ █ escape interrupt · ctrl+c/ctrl+d clear/exit · / commands · ! bash · ctrl+o more")
        || lines.get(3) != Some(&" Press ctrl+o to show full startup help and loaded resources.")
        || lines.get(5) != Some(&" Pi can explain its own features and look up its docs. Ask it how to use or extend Pi.") {
        return None;
    }
    let (footer, before_footer) = lines.split_last()?;
    let tokens: Vec<_> = footer.split_whitespace().collect();
    let zero_usage = match tokens.as_slice() {
        ["$0.000", "(sub)", "0.0%/272k", "(auto)", "gpt-6-luna", "•", "low"] => true,
        ["↑1.1k", "↓5", "$0.000", "(sub)", "0.4%/272k", "(auto)", "gpt-6-luna", "•", "low"] => {
            false
        }
        _ => return None,
    };
    let (cwd, before_cwd) = before_footer.split_last()?;
    let (lower, before_lower) = before_cwd.split_last()?;
    if !cwd.starts_with('/') || !rule(lower) {
        return None;
    }
    let top = before_lower
        .iter()
        .rposition(|line| line.starts_with("──"))?;
    let upper = *lines.get(top)?;
    let turn = if upper == *lower {
        TurnState::NotRunning
    } else if upper.strip_prefix("── ⠇ Working ").is_some_and(rule)
        && upper.chars().count() == lower.chars().count()
    {
        TurnState::Running
    } else {
        return None;
    };
    let input = match before_lower.get(top + 1..)? {
        [""] => InputState::Empty,
        [] => return None,
        _ => InputState::Draft,
    };
    let fresh_editor = if input == InputState::Empty
        && turn == TurnState::NotRunning
        && zero_usage
        && startup_only(lines.get(6..top)?)
    {
        FreshEditorEvidence::Present
    } else {
        FreshEditorEvidence::Unknown
    };
    Some(ScreenFacts {
        input,
        turn,
        dialog: DialogState::None,
        fresh_editor,
    })
}

fn startup_only(history: &[&str]) -> bool {
    let body: Vec<_> = history
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    let Some(extensions) = body.iter().position(|line| *line == "[Extensions]") else {
        return false;
    };
    // Only captured resource lists and startup endings, never a notice anywhere in history.
    let tail = body.get(extensions + 1..).unwrap_or_default();
    let warning = "Warning: tmux extended-keys is off. Modified Enter keys may not work. Add `set -g extended-keys on` to ~/.tmux.conf";
    match tail {
        ["pi-atm.ts", "✓ New session started"] => true,
        ["pi-atm.ts", first, "and restart tmux."] if *first == warning => true,
        ["obra/superpowers:.pi/extensions/superpowers.ts, pi-atm.ts", first, "and restart tmux.", upper, "Update Available", "New version 1.5.2 is available. Run pi update", "Changelog: https://pi.dev/changelog", lower]
            if *first == warning && rule(upper) && upper == lower =>
        {
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    macro_rules! capture {
        ($name:literal) => {
            include_str!(concat!("../tests/fixtures/screen_pi-", $name, "_1.5.0.txt"))
        };
    }
    const IDLE: &str = capture!("after-clear");
    const FRESH: &str = capture!("clear-poll-01");
    const STARTUP: &str = capture!("startup");
    const RUNNING: &str = capture!("before-escape");

    // Captured: pi 1.5.0, 2026-11-14, U5 private-tmux capture; sanitized.
    fn cases() -> Vec<(&'static str, &'static str, ScreenFacts)> {
        use DialogState::{None, Unknown as DialogUnknown};
        use FreshEditorEvidence::{Present, Unknown as NotFresh};
        use InputState::{Empty, Unknown};
        use TurnState::{NotRunning, Running, Unknown as TurnUnknown};
        let mut cases = Vec::new();
        macro_rules! case {
            ($name:literal, $input:expr, $turn:expr, $dialog:expr, $fresh:expr) => {
                cases.push((
                    $name,
                    capture!($name),
                    ScreenFacts {
                        input: $input,
                        turn: $turn,
                        dialog: $dialog,
                        fresh_editor: $fresh,
                    },
                ));
            };
        }
        case!("startup", Empty, NotRunning, None, Present);
        case!("trust-startup", Empty, NotRunning, None, Present);
        case!("arrival", Empty, NotRunning, None, NotFresh);
        case!("after-clear", Empty, NotRunning, None, NotFresh);
        case!("escape-5", Empty, NotRunning, None, NotFresh);
        case!("before-escape", Empty, Running, None, NotFresh);
        case!("clear-poll-01", Empty, NotRunning, None, Present);
        case!("shell", Unknown, TurnUnknown, DialogUnknown, NotFresh);
        case!("final-shell", Unknown, TurnUnknown, DialogUnknown, NotFresh);
        case!(
            "trust-final-shell",
            Unknown,
            TurnUnknown,
            DialogUnknown,
            NotFresh
        );
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
    fn control_policy_uses_captured_commands() {
        assert_eq!(CLEAR_COMMAND, "/new");
        assert!(LAUNCH_POLICY.required_args.is_empty());
        assert_eq!(LAUNCH_POLICY.model_flag, "--model");
    }

    #[test]
    fn startup_interrupt_help_is_not_running() {
        assert_eq!(observe_screen(STARTUP, None).turn, TurnState::NotRunning);
        assert_eq!(observe_screen(RUNNING, None).turn, TurnState::Running);
    }

    #[test]
    fn stale_new_session_notice_is_not_fresh() {
        assert_eq!(
            observe_screen(IDLE, None).fresh_editor,
            FreshEditorEvidence::Unknown
        );
        // Synthetic: zero usage alone cannot erase conversation after the old notice.
        let text = IDLE.replace(
            "↑1.1k ↓5 $0.000 (sub) 0.4%/272k (auto)",
            "$0.000 (sub) 0.0%/272k (auto)",
        );
        assert_eq!(
            observe_screen(&text, None).fresh_editor,
            FreshEditorEvidence::Unknown
        );
    }

    #[test]
    fn footer_or_border_loss_is_unknown() {
        // Synthetic: cropped viewport, missing footer, missing border, unknown model/footer.
        let lines: Vec<_> = IDLE.lines().collect();
        for text in [
            lines[2..].join("\n"),
            lines[..39].join("\n"),
            IDLE.replacen(&"─".repeat(120), "", 1),
            IDLE.replace("(auto)", "(novel)"),
        ] {
            assert_eq!(observe_screen(&text, None), ScreenFacts::default());
        }
    }

    #[test]
    fn unknown_border_status_is_unknown() {
        // Synthetic: unsupported progress UI must not fall back to idle.
        for status in ["⟳ Compaction", "⠇ Waiting", "⠇ Working extra"] {
            let text = RUNNING.replace("⠇ Working", status);
            assert_eq!(observe_screen(&text, None), ScreenFacts::default());
        }
    }

    #[test]
    fn shell_suffix_invalidates_old_editor() {
        // Synthetic: unknown dialog/shell appended below a complete historical editor.
        for suffix in ["shell$ ", "Approve?\nYes\nNo", "Unknown extension UI"] {
            assert_eq!(
                observe_screen(&format!("{IDLE}{suffix}"), None),
                ScreenFacts::default()
            );
        }
    }

    #[test]
    fn whitespace_unicode_and_multiline_drafts_are_not_empty() {
        // Synthetic: no real free-text pi draft was captured. Do not erase user content.
        let border = "─".repeat(120);
        for draft in [
            " ",
            "  ",
            "\u{a0}",
            "hello\nworld",
            "\n",
            "日本語🦀",
            "\u{200b}",
        ] {
            let text = FRESH.replace(
                &format!("{border}\n\n{border}"),
                &format!("{border}\n{draft}\n{border}"),
            );
            let facts = observe_screen(&text, None);
            assert_eq!(facts.input, InputState::Draft, "{draft:?}");
            assert_eq!(facts.fresh_editor, FreshEditorEvidence::Unknown);
        }
    }

    #[test]
    fn controls_cursor_and_unknown_screens_refuse_observation() {
        // Synthetic: controls and cursor metadata weren't captured; never infer emptiness.
        for control in ['\x1b', '\r', '\t', '\0', '\x7f'] {
            assert_eq!(
                observe_screen(&format!("{IDLE}{control}"), None),
                ScreenFacts::default()
            );
        }
        assert_eq!(
            observe_screen(IDLE, Some(ScreenCursor { column: 0, row: 36 })),
            ScreenFacts::default()
        );
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
        fn fixture_mutations_never_panic(case in 0usize..10, cut in any::<usize>(), inserted in any::<char>()) {
            // Synthetic: truncations at Unicode boundaries with arbitrary scalar insertion.
            let cases = cases();
            let mut chars: Vec<_> = cases[case].1.chars().collect();
            chars.truncate(cut % (chars.len() + 1));
            chars.push(inserted);
            let _ = observe_screen(&chars.into_iter().collect::<String>(), None);
        }
    }
}
