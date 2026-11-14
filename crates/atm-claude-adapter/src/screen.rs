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
//! No cursor was captured; supplying one currently returns all Unknown rather
//! than treating its position (even Home) as proof of emptiness. Ordinary unknown
//! layouts, versions, modes, overlays and incomplete dialogs also fail closed.
//!
//! Fresh layout evidence does not establish that a particular clear completed.
//! `/clear` remains in history after a successful clear. Consumers own temporal,
//! process/pane identity and copy-mode checks; identical historical text cannot
//! prove that it came from a currently live editor.

use atm_core::{
    DialogState, FreshEditorEvidence, InputState, LaunchPolicy, ScreenCursor, ScreenFacts,
    TurnState,
};

/// Submit only after the caller has established a safe active input.
pub const CLEAR_COMMAND: &str = "/clear";
/// Agent-launch policy, verified by the same capture's launch argv.
pub const LAUNCH_POLICY: LaunchPolicy = LaunchPolicy {
    required_args: &[],
    model_flag: "--model",
};

/// Recognize only complete captured terminal suffixes; unknown never means safe.
#[must_use]
pub fn observe_screen(text: &str, cursor: Option<ScreenCursor>) -> ScreenFacts {
    if cursor.is_some() || text.chars().any(|c| c.is_control() && c != '\n') {
        return ScreenFacts::default();
    }
    let mut lines: Vec<_> = text.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if let Some(dialog) = dialog(&lines) {
        return ScreenFacts {
            dialog,
            ..ScreenFacts::default()
        };
    }
    editor(&lines).unwrap_or_default()
}

fn rule(line: &str, glyph: char) -> bool {
    line.chars().take(8).count() == 8 && line.chars().all(|c| c == glyph)
}

fn header(lines: &[&str]) -> bool {
    lines.get(1) == Some(&" ▐▛███▛█   Claude Code v2.1.325")
        && lines
            .get(2)
            .is_some_and(|line| line.starts_with("▝▜██████▀  "))
        && lines
            .get(3)
            .is_some_and(|line| line.starts_with(" ▝▝   ▝▝   /"))
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
        ["Accessing workspace:", folder, question, continuation,
            "Claude Code'll be able to read, edit, and execute files here.",
            "Security guide", "❯ No, exit", "Yes, I trust this folder",
            "Enter to confirm · Esc to cancel"]
            if *question == "Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source"
                && *continuation == "project, or work from your team). If not, take a moment to review what's in this folder first."
                && folder.starts_with('/') && !folder.contains(['…', '⤶']) && !folder.contains("...") =>
        {
            Some(DialogState::FolderTrust { folder: (*folder).into() })
        }
        ["Bash command", _description, upper, _command, lower,
            "Ask rule Bash overrides auto mode for this command.",
            "/permissions to let auto mode decide", "Do you want to proceed?",
            "❯ 1. Yes", "2. No", "Esc to cancel · Tab to amend"]
            if header(lines) && rule(upper, '╌') && upper == lower =>
        {
            Some(DialogState::Permission)
        }
        _ => None,
    }
}

fn editor(lines: &[&str]) -> Option<ScreenFacts> {
    if !header(lines) {
        return None;
    }
    let (footer, before_footer) = lines.split_last()?;
    if !matches!(
        footer.trim(),
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
    if top < 7 || lines.get(top) != lines.get(lower) {
        return None;
    }
    let (first, continuation) = lines.get(top + 1..lower)?.split_first()?;
    let input = first.strip_prefix("❯\u{a0}")?;
    // Don't trim editor content: even a whitespace-only draft belongs to the user.
    let input = if !continuation.is_empty() {
        InputState::Draft
    } else if input.is_empty() {
        InputState::Empty
    } else if input.starts_with("Try \"") && input.ends_with('"') {
        InputState::Unknown
    } else {
        InputState::Draft
    };
    // Captured active status occupies the three rows immediately above the border.
    // Unknown text here may be new progress UI: absence of a known spinner isn't idle.
    let status_start = top.saturating_sub(3);
    let status: Vec<_> = lines
        .get(status_start..top)?
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    let history: Vec<_> = lines
        .get(4..status_start)?
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    // Blank status rows alone aren't idle evidence: an unfamiliar/shifted status
    // may have fallen into history. Require a captured idle history ending too.
    let idle_history = match history.last() {
        None | Some(&"❯ /clear") => true,
        Some(line) => line.starts_with("✻ ") && line.contains(" · done "),
    };
    let turn = match status.as_slice() {
        [] | ["◐ medium · /effort"] if idle_history => TurnState::NotRunning,
        ["✶ Levitating…" | "✽ Sketching… (running SessionEnd hook · 0s)", tip]
            if tip.starts_with("⎿ \u{a0}Tip: ") =>
        {
            TurnState::Running
        }
        _ => TurnState::Unknown,
    };
    let fresh_editor = if turn == TurnState::NotRunning
        && input != InputState::Draft
        && matches!(history.as_slice(), [] | ["❯ /clear"])
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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
                    ScreenFacts {
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
            lines[2..].join("\n"),
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
    fn activity_outside_captured_status_region_does_not_prove_idle() {
        // Synthetic: a shifted or partially redrawn status must not disappear into history.
        let border = "─".repeat(120);
        let shifted = capture!("before-escape").replacen(&border, &format!("\n\n\n\n{border}"), 1);
        assert_eq!(observe_screen(&shifted, None).turn, TurnState::Unknown);
        let unfamiliar = IDLE.replace(
            "✻ Crunched for 1s · done 11:25",
            "Unrecognized active operation",
        );
        assert_eq!(observe_screen(&unfamiliar, None).turn, TurnState::Unknown);
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
