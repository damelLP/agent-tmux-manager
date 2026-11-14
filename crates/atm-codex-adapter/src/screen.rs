//! Conservative screen facts for Codex 0.167.0.
//!
//! Captured: Codex 0.167.0, 2026-11-14, U5 private-tmux capture; sanitized.
//! See `tests/fixtures/README.md` for mappings and missing states. Input is the
//! current `capture-pane -p` viewport, without attributes. Controls except newline,
//! supplied cursor metadata and unsupported/incomplete layouts refuse observation.
//!
//! The placeholder may also be an identical draft: it always remains Unknown.
//! **Consumers cannot deliver into Codex under this plain-text contract yet.**
//! Cursor/attribute inference is deferred until backed by real captures. Only the
//! captured `/new` completion is a known draft. Permission/trust dialogs and
//! free-text draft layouts were not captured; no speculative recognizer is added.
//!
//! Fresh logo layout is not evidence that a particular clear completed: two U5
//! captures from different operations are byte-identical. Consumers own operation
//! transitions, pane/process identity, live viewport, copy-mode and temporal checks.

use atm_core::{
    DialogState, FreshEditorEvidence, InputState, LaunchPolicy, ScreenCursor, ScreenFacts,
    TurnState,
};

/// Submit only after the caller has established a safe active input.
pub const CLEAR_COMMAND: &str = "/new";
/// Agent launches must preserve these args, independently of legacy spawn defaults.
pub const LAUNCH_POLICY: LaunchPolicy = LaunchPolicy {
    required_args: &["--no-daemon"],
    model_flag: "-m",
};

const HEADER: &str = "  >_ OpenAI Codex (v0.167.0)";
const REVIEW_HELP: &str = "/review - review any changes and find issues";
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
pub fn observe_screen(text: &str, cursor: Option<ScreenCursor>) -> ScreenFacts {
    if cursor.is_some() || text.chars().any(|c| c.is_control() && c != '\n') {
        return ScreenFacts::default();
    }
    let mut lines: Vec<_> = text.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if known_dialog(&lines) {
        return ScreenFacts {
            dialog: DialogState::Other,
            ..ScreenFacts::default()
        };
    }
    editor(&lines).unwrap_or_default()
}

fn nonblank<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect()
}

fn header<'a>(lines: &[&'a str]) -> Option<(usize, &'a str)> {
    let index = lines.iter().position(|line| *line == HEADER)?;
    let cwd = lines.get(index + 1)?.strip_prefix("     ")?.trim_end();
    cwd.starts_with('/').then_some((index, cwd))
}

fn known_dialog(lines: &[&str]) -> bool {
    let body = nonblank(lines);
    match body.as_slice() {
        ["Update available · 0.167.0 → 0.167.1" | "Update available · 0.167.0 → 0.169.0", "Release notes: https://github.com/openai/codex/releases/latest", "› 1. Update now (runs `npm install -g @openai/codex`)", "2. Skip", "3. Skip until next version", "enter continue · esc skip"] => {
            true
        }
        [.., help, "Warnings · 1 of 1 · Startup", message, "k keep & next · esc dismiss & close · ^o copy · ←/→ warning"]
            if header(lines).is_some()
                && *help == REVIEW_HELP
                && message
                    .strip_prefix("clamping SessionEnd hook timeout to 3s in /")
                    .is_some_and(|path| path.ends_with("/.codex/hooks.json")) =>
        {
            true
        }
        _ => false,
    }
}

fn editor(lines: &[&str]) -> Option<ScreenFacts> {
    let (header_index, cwd) = header(lines)?;
    let (last, before_last) = lines.split_last()?;
    let shortcuts = last.split_whitespace().collect::<Vec<_>>().join(" ");
    let regular = matches!(
        shortcuts.as_str(),
        "← for agents · ? for shortcuts"
            | "← for agents · ? for shortcuts ⚠ 1 warning · f2 to view"
    );
    let (model, before_model) = if regular {
        before_last.split_last()?
    } else {
        (last, before_last)
    };
    let location = model.trim_end().strip_prefix("  GPT-6-Luna low · ")?;
    if location != cwd && location.strip_suffix(" · Reply ok") != Some(cwd) {
        return None;
    }
    let (separator, before_separator) = before_model.split_last()?;
    let (composer, before_composer) = before_separator.split_last()?;
    if !separator.is_empty() {
        return None;
    }
    let (input, dialog, history) = if regular && *composer == "› Ask Codex to do anything" {
        (InputState::Unknown, DialogState::None, before_composer)
    } else if !regular && *composer == "› /new" {
        let (blank, before_blank) = before_composer.split_last()?;
        let (menu, history) = before_blank.split_last()?;
        if !blank.is_empty() || *menu != "› /new  start a new chat during a conversation" {
            return None;
        }
        (InputState::Draft, DialogState::Other, history)
    } else {
        return None;
    };
    let body = nonblank(history.get(header_index + 2..)?);
    let fresh = header_index == 1 && body == LOGO && input == InputState::Unknown;
    let last_history = body.last().copied().unwrap_or_default();
    let turn = if last_history == "• Working (0s • esc to interrupt)" {
        // The captured active status is exactly three rows above the composer.
        if history.get(history.len().saturating_sub(3)) == Some(&last_history) {
            TurnState::Running
        } else {
            TurnState::Unknown
        }
    } else if fresh
        || matches!(
            last_history,
            "Worked for 1s • 11:31 AM"
                | "Worked for 2s • 11:32 AM"
                | "■ Conversation interrupted - use /feedback if something went wrong"
        )
        || last_history == REVIEW_HELP
        || status_output(&body)
    {
        TurnState::NotRunning
    } else {
        TurnState::Unknown
    };
    Some(ScreenFacts {
        input,
        turn,
        dialog,
        fresh_editor: if fresh {
            FreshEditorEvidence::Present
        } else {
            FreshEditorEvidence::Unknown
        },
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
                    ScreenFacts {
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
            lines[3..].join("\n"),
            IDLE.replace("GPT-6-Luna low", "Novel footer"),
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
    fn identical_fresh_frames_do_not_prove_new_generation() {
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
            RUNNING.replace(
                "• Working (0s • esc to interrupt)",
                "• Working (0s • esc to interrupt)\n\n\n\n",
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
