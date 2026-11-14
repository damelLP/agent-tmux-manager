//! Conservative screen facts for pi 1.5.0.
//!
//! Captured: pi 1.5.0, 2026-11-14, U5 private-tmux capture; sanitized.
//! See `tests/fixtures/README.md` for source mappings and missing states.
//! Input is a current `capture-pane -p` viewport (no attributes). Controls other
//! than newline, unverified cursor metadata, unknown footers/statuses and partial
//! frames refuse observation. Trailing blank rows may be omitted.
//!
//! Pi has no native tool approvals, but extension/core dialogs replace the editor.
//! Recognized select/input/editor hints and the reload box report Other with
//! unknown input/turn. Ambiguous nested borders refuse observation.
//! Free-text drafts have only synthetic safety coverage, not real captures.
//! Plain capture drops trailing spaces: whitespace-only drafts can read Empty.
//! Synthetic ASCII-space tests only check preservation of supplied text; styled
//! capture with retained trailing spaces is a separate follow-up.
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
    let (footer, before_footer) = lines.split_last()?;
    let zero_usage = usage_footer(footer)?;
    let (cwd, before_cwd) = before_footer.split_last()?;
    let (lower, before_lower) = before_cwd.split_last()?;
    if !(cwd.trim_end() == "~" || cwd.starts_with('/') || cwd.starts_with("~/")) || !rule(lower) {
        return None;
    }
    let top = before_lower
        .iter()
        .rposition(|line| line.starts_with("──"))?;
    let body = before_lower.get(top + 1..)?;
    if editor_dialog(body) {
        let mut facts = ScreenFacts::default();
        facts.dialog = DialogState::Other;
        return Some(facts);
    }
    // A pasted rule can masquerade as the last upper border. Without an editor
    // marker, earlier rules are ambiguous unless they belong to known startup UI.
    let history = lines.get(..top)?;
    if history.contains(lower) && !startup_only(history) {
        return None;
    }
    let upper = *lines.get(top)?;
    let turn = if upper == *lower {
        TurnState::NotRunning
    } else if upper
        .strip_prefix("── ")
        .and_then(|s| s.split_once(" Working "))
        .is_some_and(|(frame, rest)| {
            frame.chars().count() == 1 && "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(frame) && rule(rest)
        })
        && upper.chars().count() == lower.chars().count()
    {
        TurnState::Running
    } else {
        return None;
    };
    let input = match body {
        [""] => InputState::Empty,
        [] => return None,
        _ => InputState::Draft,
    };
    let fresh_editor = if input == InputState::Empty
        && turn == TurnState::NotRunning
        && zero_usage
        && startup_only(lines.get(..top)?)
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
    Some(facts)
}

fn editor_dialog(body: &[&str]) -> bool {
    body.iter().any(|line| {
        let hints: Vec<_> = line.trim().split("  ").collect();
        hints
            .iter()
            .any(|hint| hint.ends_with(" select") || hint.ends_with(" submit"))
            && hints.iter().any(|hint| hint.ends_with(" cancel"))
    }) || body
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        == "Reloading keybindings, extensions, skills, prompts, themes, and context files..."
}

fn decimal(value: &str) -> Option<f64> {
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    value.parse::<f64>().ok().filter(|n| n.is_finite())
}

fn count(value: &str) -> Option<f64> {
    let number = value
        .chars()
        .last()
        .filter(|c| matches!(c, 'k' | 'K' | 'M' | 'B'))
        .and_then(|unit| value.strip_suffix(unit))
        .unwrap_or(value);
    decimal(number)
}

fn usage_footer(line: &str) -> Option<bool> {
    let tokens: Vec<_> = line.split_whitespace().collect();
    let context_index = tokens
        .iter()
        .position(|token| token.contains("%/") || token.starts_with("?/"))?;
    let (percent, window) = tokens.get(context_index)?.split_once('/')?;
    count(window)?;
    let mut zero = if percent == "?" {
        false
    } else {
        decimal(percent.strip_suffix('%')?)? == 0.0
    };
    let mut rest = tokens.get(..context_index)?;
    // Each counter is emitted independently, in this fixed order, only when nonzero.
    for prefix in ["↑", "↓", "R", "W"] {
        if let Some((token, tail)) = rest
            .split_first()
            .filter(|(token, _)| token.starts_with(prefix))
        {
            zero &= count(token.strip_prefix(prefix)?)? == 0.0;
            rest = tail;
        }
    }
    if let Some((hit, tail)) = rest.split_first().filter(|(s, _)| s.starts_with("CH")) {
        let hit = decimal(hit.strip_prefix("CH")?.strip_suffix('%')?)?;
        if hit > 100.0 {
            return None;
        }
        zero &= hit == 0.0;
        rest = tail;
    }
    if let Some((cost, tail)) = rest.split_first().filter(|(s, _)| s.starts_with('$')) {
        zero &= decimal(cost.strip_prefix('$')?)? == 0.0;
        rest = tail.strip_prefix(&["(sub)"]).unwrap_or(tail);
    }
    if !rest.is_empty() {
        return None;
    }
    let model = tokens.get(context_index + 1..)?;
    let model = model.strip_prefix(&["(auto)"]).unwrap_or(model);
    let model = model.strip_prefix(&["•", "xp"]).unwrap_or(model);
    // footer.js separates the right-aligned model with at least two spaces.
    // Do not mistake an unknown left-side marker for a provider prefix.
    let aligned: Vec<_> = line
        .trim()
        .split_once("  ")
        .map(|(_, right)| right.split_whitespace().collect())
        .unwrap_or_default();
    if model != aligned {
        return None;
    }
    // Narrow panes may omit the entire right side, but never the context anchor.
    if model.is_empty() {
        return Some(zero);
    }
    let model = match model {
        [provider, tail @ ..] if provider.starts_with('(') && provider.ends_with(')') => tail,
        _ => model,
    };
    let models: Vec<_> = model.split(|token| *token == "→").collect();
    (models.len() <= 2 && models.iter().all(|part| model_label(part))).then_some(zero)
}

fn model_label(tokens: &[&str]) -> bool {
    let (name, rest) = match tokens.split_first() {
        Some(parts) => parts,
        None => return false,
    };
    !name.is_empty()
        && !name.contains(['(', ')', '•', '→'])
        && match rest {
            [] | ["•", "thinking", "off"] => true,
            ["•", effort] => {
                !effort.is_empty() && effort.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
            }
            _ => false,
        }
}

fn startup_only(history: &[&str]) -> bool {
    let rows: Vec<_> = history
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .collect();
    let [(.., title), (.., help), (.., resources), (end, description), ..] = rows.as_slice() else {
        return false;
    };
    let Some(version) = title.strip_prefix(" ▀▀█  v") else {
        return false;
    };
    if !version.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        || **help != " █▀ █ escape interrupt · ctrl+c/ctrl+d clear/exit · / commands · ! bash · ctrl+o more"
        || **resources != " Press ctrl+o to show full startup help and loaded resources."
        || **description != " Pi can explain its own features and look up its docs. Ask it how to use or extend Pi." {
        return false;
    }
    // Only fresh evidence needs history. Ordinary editor facts work with a scrolled-off header.
    // Startup resources/notices are optional; conversation after them is not startup chrome.
    history
        .get(end + 1..)
        .unwrap_or_default()
        .split(|line| line.trim().is_empty())
        .all(|block| {
            let body: Vec<_> = block.iter().map(|s| s.trim()).collect();
            match body.as_slice() {
                [] | ["✓ New session started"] => true,
                ["[Context]" | "[Skills]" | "[Prompts]" | "[Extensions]", entries @ ..] => {
                    !entries.is_empty()
                }
                [first, ..] if first.starts_with("Warning: ") => true,
                [upper, "Update Available", message, changelog, lower] => {
                    rule(upper)
                        && upper == lower
                        && message.starts_with("New version ")
                        && message.ends_with(" is available. Run pi update")
                        && changelog.starts_with("Changelog: https://")
                }
                _ => false,
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    macro_rules! facts {
        ($($field:ident: $value:expr),* $(,)?) => {{
            let mut facts = ScreenFacts::default();
            $(facts.$field = $value;)*
            facts
        }};
    }

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
                    facts! {
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
    fn coordinator_captures_with_cache_and_spinner_variants() {
        // Captured: pi 1.5.0, 2026-11-14, coordinator read-only tmux capture-pane -p; sanitized.
        for (text, turn) in [
            (capture!("extra-idle-1"), TurnState::NotRunning),
            (capture!("extra-idle-2"), TurnState::NotRunning),
            (capture!("extra-idle-3"), TurnState::NotRunning),
            (capture!("extra-working-1"), TurnState::Running),
            (capture!("extra-working-2"), TurnState::Running),
            (capture!("extra-working-3"), TurnState::Running),
        ] {
            assert_eq!(
                observe_screen(text, None),
                facts! {
                    input: InputState::Empty,
                    turn: turn,
                    dialog: DialogState::None,
                    fresh_editor: FreshEditorEvidence::Unknown
                }
            );
        }
    }

    #[test]
    fn value_variations_preserve_facts() {
        for (name, text, expected) in cases() {
            // Synthetic: value variation of each named U5 fixture, preserving zero-usage evidence.
            let varied = text
                .replace("v1.5.0", "v12.34.567")
                .replace("gpt-6-luna • low", "provider/other-model • xhigh")
                .replace(
                    "↑1.1k ↓5 $0.000 (sub) 0.4%/272k",
                    "↑75k ↓11k R1.2M CH99.7% $2.485 (sub) 30.3%/1M",
                )
                .replace("0.0%/272k", "0%/512k")
                .replace("pi-atm.ts", "another-extension.ts, custom.ts")
                .replace("New version 1.5.2", "New version 12.34.568")
                .replace("/tmp/u8a-fixture/cwd/pi", "~/other-project (feature/topic)");
            assert_eq!(observe_screen(&varied, None), expected, "{name}");
        }
        // Synthetic: value variation of before-escape, all frames in pi's braille spinner.
        for frame in "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".chars() {
            assert_eq!(
                observe_screen(&RUNNING.replace('⠇', &frame.to_string()), None).turn,
                TurnState::Running
            );
        }
        // Synthetic: resource-list omission in clear-poll-01; no extension is required for freshness.
        assert_eq!(
            observe_screen(&FRESH.replace("[Extensions]\n  pi-atm.ts", ""), None).fresh_editor,
            FreshEditorEvidence::Present
        );
    }

    #[test]
    fn damaged_footer_and_nonzero_fresh_usage_refuse_readiness() {
        // Synthetic: structural damage of after-clear, not mere value changes.
        for footer in [
            "a prose footer",
            "↑wat ↓11k $2 (sub) 30%/272k (auto) model • high",
            "$2 (sub) bad%/272k (auto) model • high",
            "$NaN (sub) 0%/272k (auto) model • high",
            "$2 (sub) 3%/272k (auto) • high",
        ] {
            let mut lines: Vec<_> = IDLE.lines().collect();
            *lines.last_mut().unwrap() = footer;
            assert_eq!(
                observe_screen(&lines.join("\n"), None),
                ScreenFacts::default()
            );
        }
        // Synthetic: value variation of clear-poll-01; zero usage is real freshness evidence.
        for text in [
            FRESH.replace("$0.000", "$0.123"),
            FRESH.replace("0.0%/", "1.0%/"),
            FRESH.replace("$0.000", "↑1 ↓0 R0 CH0% $0.000"),
            FRESH.replace("$0.000", "CH99% $0.000"),
        ] {
            assert_eq!(
                observe_screen(&text, None).fresh_editor,
                FreshEditorEvidence::Unknown
            );
        }
    }

    #[test]
    fn rendered_footer_variants_preserve_editor_facts() {
        // Synthetic: rendered from pi 1.5.0's footer.js; no real capture.
        // Order: default, W, up-only, down-only, unknown context, thinking off,
        // non-reasoning, no cost, API cost, no auto, provider, routed, no model,
        // truncated stats (refuse), stats-only, overfull context, experimental, zero window.
        let expected_zero = [
            Some(true),
            Some(false),
            Some(false),
            Some(false),
            Some(false),
            Some(true),
            Some(true),
            Some(true),
            Some(false),
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            None,
            Some(true),
            Some(false),
            Some(true),
            Some(true),
        ];
        let footers: Vec<_> = capture!("footers").lines().skip(1).collect();
        assert_eq!(footers.len(), expected_zero.len());
        for (footer, zero) in footers.into_iter().zip(expected_zero) {
            let mut lines: Vec<_> = FRESH.lines().collect();
            *lines.last_mut().unwrap() = footer;
            let text = lines.join("\n").replace("/tmp/u8a-fixture/cwd/pi", "~");
            let facts = observe_screen(&text, None);
            if let Some(zero) = zero {
                assert_eq!(facts.input, InputState::Empty, "{footer}");
                assert_eq!(facts.turn, TurnState::NotRunning, "{footer}");
                assert_eq!(facts.dialog, DialogState::None, "{footer}");
                assert_eq!(
                    facts.fresh_editor,
                    if zero {
                        FreshEditorEvidence::Present
                    } else {
                        FreshEditorEvidence::Unknown
                    },
                    "{footer}"
                );
            } else {
                assert_eq!(facts, ScreenFacts::default(), "{footer}");
            }
        }
    }

    #[test]
    fn startup_header_is_located_by_its_chrome() {
        // Synthetic: move U5 startup header and remove only its blank spacing.
        let compact = FRESH
            .lines()
            .take(6)
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let tail = FRESH.lines().skip(6).collect::<Vec<_>>().join("\n");
        for blanks in [0, 1, 6] {
            let text = format!("{}{compact}\n{tail}", "\n".repeat(blanks));
            assert_eq!(
                observe_screen(&text, None).fresh_editor,
                FreshEditorEvidence::Present
            );
        }
    }

    #[test]
    fn rendered_dialogs_never_become_editor_facts() {
        // Synthetic: rendered from pi 1.5.0's components named in each fixture; no real capture.
        // Strip the provenance label, then replace only the real idle editor slot.
        let border = "─".repeat(120);
        let (history, _) = IDLE.split_once(&border).unwrap();
        let (_, footer) = IDLE.rsplit_once(&border).unwrap();
        for name_and_text in [
            ("select", capture!("dialog-select")),
            ("confirm", capture!("dialog-confirm")),
            ("input", capture!("dialog-input")),
            ("editor", capture!("dialog-editor")),
            ("missing-cwd", capture!("dialog-missing-cwd")),
            ("summarize", capture!("dialog-summarize")),
            ("reload", capture!("dialog-reload")),
        ] {
            let (name, labelled) = name_and_text;
            let (_, body) = labelled.split_once('\n').unwrap();
            let text = format!("{history}{}{footer}", body.trim_end_matches('\n'));
            let facts = observe_screen(&text, None);
            assert_eq!(facts.input, InputState::Unknown, "{name}");
            assert_eq!(facts.turn, TurnState::Unknown, "{name}");
            assert!(
                matches!(facts.dialog, DialogState::Other | DialogState::Unknown),
                "{name}"
            );
            assert_eq!(facts.fresh_editor, FreshEditorEvidence::Unknown, "{name}");
            // Synthetic: custom keybindings alter key names, not the hint's action words.
            let varied = text
                .replace("enter select", "ctrl+x select")
                .replace("enter submit", "ctrl+x submit")
                .replace("escape/ctrl+c cancel", "alt+q cancel");
            assert_eq!(observe_screen(&varied, None), facts, "{name} bindings");
        }
    }

    #[test]
    fn draft_with_a_full_width_rule_cannot_look_empty() {
        // Synthetic: pasted editor content imitates the upper border, ending with an empty row.
        let border = "─".repeat(120);
        for draft in [format!("draft\n{border}\n"), format!("draft\n\n{border}\n")] {
            let text = FRESH.replace(
                &format!("{border}\n\n{border}"),
                &format!("{border}\n{draft}\n{border}"),
            );
            let facts = observe_screen(&text, None);
            assert_ne!(facts.input, InputState::Empty);
            assert_eq!(facts.fresh_editor, FreshEditorEvidence::Unknown);
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
            lines[..37].join("\n"),
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
        // Synthetic: no real free-text pi draft was captured. Do not erase supplied content.
        // capture-pane -p removes trailing ASCII spaces: these cases do not prove
        // end-to-end protection for real whitespace-only drafts.
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
