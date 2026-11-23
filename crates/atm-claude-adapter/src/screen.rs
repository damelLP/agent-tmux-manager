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
//! refuses multiline/wrapped reconstruction. Trailing ASCII spaces are chrome:
//! a sealed 2.1.325 probe confirmed `/clear   ` executes clear, not a prompt.
//! The queued-message placeholder uses the same inverse-first-cell/faint-rest
//! evidence as suggestions; other faint text is not a typed draft.
//!
//! Captured: Claude Code 2.1.325, 2026-11-15, rehearsal on a candidate and a sealed
//! private tmux with nothing submitted, tmux 3.8 `capture-pane -p -e -N`.
//! With its native cursor Claude shows the terminal's own cursor and draws no
//! inverse cell, with or without a focused tmux client. The 2.1.325 binary turns
//! it on for `CLAUDE_CODE_NATIVE_CURSOR`, `CLAUDE_CODE_ACCESSIBILITY` or the
//! account's `tengu_native_cursor` flag (read, not captured both ways); the
//! offline captures above had it off. A suggestion row is then faint in every
//! non-space cell: the startup `Try "…"`, and the free-text suggestion Claude
//! shows after a turn. Such a single row is Empty whatever its text. Typed text
//! was never faint, including the placeholder's own text and its first letter;
//! typing removed the startup placeholder. Mixed rows remain Unknown.
//!
//! Captured: Claude Code 2.1.325, 2026-11-16, sealed loopback-only private tmux
//! 3.8 capture-pane -p -e -N with canned Messages SSE; capture root anonymized.
//! Both native and drawn cursor modes were verified.
//! A drawn free-text suggestion has an inverse,
//! non-faint first glyph and a nonempty wholly faint, non-inverse suffix. That
//! single-row proof also reads Empty, never a staged draft. At 56 columns both
//! fullscreen and classic truncate long suggestions with an ellipsis; a real
//! two-row ghost remains unverified and every multiline editor stays Draft.
//! The same sealed capture also establishes the right-aligned `new task? /clear
//! to save N tokens` contextual hint at 80/116/160 columns in both cursor modes.
//! Counts follow Claude Code 2.1.325's compact number formatter (en-US compact
//! notation, lowercased, with a trailing `.0` removed), accepting optional k/m/b/t
//! suffixes and at most one decimal digit.
//! Only its exact shape and two-cell right margin directly above the upper rule
//! are chrome, not turn evidence. Typed drafts stay Draft; a spinner or unknown
//! transcript tail above the hint still decides the turn. First-turn auto-mode
//! prose, other notices and extra footer rows remain unsupported.
//!
//! Captured: Claude Code 2.1.325, 2026-11-15, sealed tmux capture-pane -p -e,
//! custom statusLine. At 80, 116 and 160 columns a custom status line occupies
//! one two-space-indented row immediately below the editor's lower rule and
//! above the exact mode footer. Its values are opaque, never input or turn
//! evidence. Only that optional slot is accepted; a nonblank second row, a
//! displaced/unindented row or unknown footer still refuses. Multiline status
//! output and teammate lists below the footer are not established by these
//! captures.
//!
//! Captured: Claude Code 2.1.327, 2.1.329 and 2.1.330, 2026-11-20, sealed rig
//! (private user and network namespace, canned Messages API), tmux 3.8
//! `capture-pane -p -e -N` at 82x60 (s3-stuckseat). A prompt suggestion in a
//! pane without terminal focus is wholly faint with no inverse cursor glyph
//! (Claude draws that glyph only while focused); it reads Empty, as above.
//! Left unfocused for 180 s after its third turn, an idle seat appends an away
//! recap: a `※ recap: ` row (bold label, italic text), wrapped onto rows
//! indented two cells, ending `(turn off recaps in /config)` on the first few.
//! It is chrome above the hint, never turn evidence; any other row after it
//! keeps the turn Unknown. A live 82-column seat (version not recorded,
//! read-only, not committed) showed the `/clear` hint cut to fit beside the
//! update notice, `new task? /clear to save 480.5k toke… · ✔ Update installed ·
//! /restart to apply`: a whole count, a cut ` tokens` and `…` are accepted; a
//! cut count is not.
//!
//! Typing `/clear` can replace the footer with a slash menu (P1 B-clear-001).
//! Captured: Claude Code 2.1.325, 2026-11-15, sealed tmux capture-pane -p -e -N, no prompt.
//! Fullscreen also places the menu above the editor, with other matching skills
//! and aligned wrapped descriptions. It is recognized without a startup header;
//! its transcript tail still proves neither idle nor freshness. Both completion
//! layouts require the editor and selected command exact before Enter. A present
//! but invalid menu refuses staging and control, never an ordinary-editor fallback.
//! The control observer additionally recognizes captured launch/footer modes.
//!
//! Captured: Claude Code 2.1.325, 2026-11-14, U5b private tmux 100x30 capture-pane -p; sanitized.
//! Header, bordered input and footer are found by chrome, not absolute rows.
//! Status is the transcript tail, independent of blank spacing. Right-aligned
//! footer notices and the deleted-text hint are not turn evidence. Separate
//! focus-warning and unseen update rows remain unsupported and return Unknown.
//!
//! Captured: Claude Code 2.1.325, 2026-11-15, read-only styled tmux lead captures.
//! A shell-count footer and the exact right-aligned native auto-update failure
//! immediately above the upper editor border are recognized. The installed
//! 2.1.325 bundle confirms `1 shell` / `N shells`, auto/manual mode and an optional
//! `← for agents` before or after the count. Counts are canonical positive ASCII
//! digits, not a fixed numeric range. Monitor mixtures, other task labels and
//! selectors below the footer stay Unknown. The notice keeps its observed two-cell
//! right margin relative to the border; neither its text elsewhere nor a novel
//! notice is skipped. These forms do not change draft, turn or freshness rules.
//! Manual-mode parsing belongs to the control consumer, so plain manual mode stays
//! Unknown in the legacy observer; `⏸ manual mode on · N shells` counts because
//! the pill frame is the bundle's own.
//! Pill attributes rely on the 2.1.325 bundle's routing: with footer focus, a typed
//! key or paste goes to the prompt, so staged-then-Enter submission still lands there.
//!
//! Captured: Claude Code 2.1.325, 2026-11-17, read-only tmux capture-pane -p -e -N
//! of an idle seat (supplied by the lead), redacted. `✔ Update installed · /restart
//! to apply` appears alone or after the clear-context hint's ` tokens · `.
//! Only these exact, right-aligned pre-border shapes are chrome; the remaining
//! transcript still proves turn state. The captured 184x11 views have no mode
//! footer and remain Unknown; footer-added conformance variants are Synthetic.
//!
//! Captured: Claude Code 2.1.325 (P3, 2026-11-14), 2.1.327 and 2.1.330 (sealed
//! rig, 2026-11-20/21), capture-pane -p -e (s3-5c). Ctrl-U clears the editor's
//! cursor row: a one-row draft entirely (atmd sends it as Claude's retract key);
//! a wrapped draft keeps its other rows. Then a right-aligned `ctrl+y to paste
//! deleted text` row stands above the editor for 5.1 s (2.1.327, 2.1.330). It is
//! not read as chrome (the 2.1.326 post-clear frame must stay unproven), so the
//! turn reads Unknown meanwhile.
//!
//! Captured: Claude Code 2.1.327, 2026-11-21, sealed rig capture-pane -p -e
//! (s3-busy). Once a seat is idle, the `/clear` hint stands on the same
//! right-aligned row before ` · ` and an update notice: `✘ Auto-update failed ·
//! Run claude doctor` (captured) or `✔ Update installed · Restart to update`
//! (read on a live 2.1.327 seat; tested on a synthetic row). Both, like
//! `/restart to apply`, are chrome there; any other notice stays unknown.
//!
//! Captured: Claude Code 2.1.327, 2026-11-22, sealed real tmux paste probe.
//! A bracketed paste starting with `[` closed the optional session feedback
//! survey unanswered and reached the composer unchanged. A bracketed paste of
//! a lone `0` selected Dismiss; the framing alone did not prevent a choice.
//! The opt-in readiness is for an inbox pointer paste only, not for Enter.
//!
//! Fresh layout evidence does not establish that a particular clear completed.
//! `/clear` remains in history after a successful clear. Consumers own temporal,
//! process/pane identity and copy-mode checks; identical historical text cannot
//! prove that it came from a currently live editor.

mod control;
pub use control::{observe_control_screen, CONTROL_POLICY};

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

/// Opt-in evidence for a daemon inbox pointer paste, not a general safe-editor read.
#[must_use]
pub fn inbox_survey_ready(input: ScreenInput<'_>) -> bool {
    inspect(input, None).2
}

const SURVEY_REFUSAL: &str =
    "a feedback survey is open; answer or dismiss it in the pane, then retry";

/// Refuse risky input only on the exact captured survey layout recognized by
/// the legacy observer. It fails open with `⏸ manual mode on` or another
/// unrecognized footer, an unrecognized notice between the survey pair and the
/// editor border, a pane too narrow to keep the heading or choices on one row,
/// or a frame the styled decoder rejects. A match never grants input permission.
#[must_use]
pub fn legacy_send_veto(
    input: ScreenInput<'_>,
    text: &str,
    bracketed: Option<bool>,
) -> Option<&'static str> {
    let (facts, draft, _, survey_visible) = inspect(input, None);
    let mut chars = text.chars();
    let raw = bracketed != Some(true);
    // Treat a whitespace-only suffix as a choice; paste trimming is unverified.
    let selecting_paste =
        chars.next().is_some_and(survey_choice) && (raw || chars.all(char::is_whitespace));
    (survey_visible
        && (choice_draft(facts.input, draft.as_deref())
            || selecting_paste
            || (raw && text.contains(['\t', '\r', '\n']))))
    .then_some(SURVEY_REFUSAL)
}

/// A numbered reply can recall a choice draft before its final Enter. This veto
/// has the same recognized layout and fail-open cases as [`legacy_send_veto`].
#[must_use]
pub fn legacy_submit_veto(input: ScreenInput<'_>, moves: bool) -> Option<&'static str> {
    let (facts, draft, _, survey_visible) = inspect(input, None);
    (survey_visible && (moves || choice_draft(facts.input, draft.as_deref())))
        .then_some(SURVEY_REFUSAL)
}

// Read offline (not captured): Claude Code 2.1.327 bundle, 2026-11-22;
// unchanged through 2.1.330. The submit handler accepts a single UTF-16 code
// unit: digits 0–3 after NFKC (listed here), or one of its layout aliases.
fn survey_choice(c: char) -> bool {
    "0123０１２３₀₁₂₃①②③⓪⁰¹²³&é\"à".contains(c)
}

fn choice_draft(input: InputState, draft: Option<&str>) -> bool {
    input != InputState::Empty
        && draft.is_none_or(|draft| {
            let mut chars = draft.trim_end_matches(' ').chars();
            chars.next().is_some_and(survey_choice) && chars.next().is_none()
        })
}

// The booleans are inbox paste readiness and separate survey visibility.
type EditorRead = (ScreenFacts, Option<String>, bool, bool);

fn inspect(input: ScreenInput<'_>, cursor: Option<ScreenCursor>) -> EditorRead {
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
    let (facts, draft, inbox_ready, survey_visible) = inspect_prepared(text, styled);
    let menu = inbox_ready
        .then_some(styled)
        .flatten()
        .map(control::completion_draft);
    (
        facts,
        draft,
        matches!(menu, Some(control::CompletionDraft::Absent)),
        survey_visible,
    )
}

fn inspect_prepared(text: &str, styled: Option<&StyledScreen>) -> EditorRead {
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
        return (facts, None, false, false);
    }
    editor(&lines, styled).unwrap_or_default()
}

/// Compare only a reconstructable active single-line draft, never history.
///
/// Requires Styled and a nonempty expected string without controls or trailing
/// whitespace. Compares after removing trailing ASCII spaces from the draft only;
/// leading whitespace stays exact. A captured completion menu matches only
/// when both the editor draft and selected command equal expected exactly;
/// its description is not command identity. Other differences return Some(false);
/// unknown layouts, faint text and multiline/wrapped drafts return None. A match proves
/// visible text only, not that Enter submits it: consumers must handle popups
/// and refuse both Some(false) and None.
#[must_use]
pub fn staged_draft_matches(input: ScreenInput<'_>, expected: &str) -> Option<bool> {
    if !matches!(input, ScreenInput::Styled(_))
        || expected.is_empty()
        || expected.ends_with(char::is_whitespace)
        || expected.chars().any(char::is_control)
    {
        return None;
    }
    if let ScreenInput::Styled(text) = input {
        #[cfg(test)]
        DECODE_COUNT.with(|count| count.set(count.get() + 1));
        let screen = StyledScreen::parse(text).ok()?;
        match control::completion_draft(&screen) {
            control::CompletionDraft::Recognised { draft, selected } => {
                return Some(draft == expected && selected == expected);
            }
            control::CompletionDraft::Invalid => return None,
            control::CompletionDraft::Absent => {}
        }
    }
    let (_, draft, ..) = inspect(input, None);
    let draft = draft?;
    Some(draft.trim_end_matches(' ') == expected)
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

fn editor(lines: &[&str], styled: Option<&StyledScreen>) -> Option<EditorRead> {
    let header_end = header_end(lines);
    if let Some((before_footer, footer)) =
        styled.and_then(|s| control::background_agent_footer(lines, s))
    {
        let mut read = editor_facts(lines, styled, header_end, before_footer, footer)?;
        // The selector can obscure input focus. Preserve positive running
        // evidence only; neither staging nor the survey paste opt-in is safe.
        if read.0.turn != TurnState::Running {
            read.0.turn = TurnState::Unknown;
        }
        read.0.fresh_editor = FreshEditorEvidence::Unknown;
        read.1 = None;
        read.2 = false;
        return Some(read);
    }
    let (footer, before_footer) = lines.split_last()?;
    let (footer, before_footer) = if footer.trim() == "Ctrl+Y to paste deleted text" {
        before_footer.split_last()?
    } else {
        (footer, before_footer)
    };
    let footer = control::editor_footer(footer)?;
    editor_facts(lines, styled, header_end, before_footer, footer)
}

fn editor_facts(
    lines: &[&str],
    styled: Option<&StyledScreen>,
    header_end: Option<usize>,
    before_footer: &[&str],
    footer: &str,
) -> Option<EditorRead> {
    let lower = before_footer.iter().rposition(|line| rule(line, '─'))?;
    let below = before_footer.get(lower + 1..)?;
    // The captured status slot is adjacent to the rule, not arbitrary footer text.
    let below = match below.split_first() {
        Some((row, rest)) if row.starts_with("  ") && !row.trim().is_empty() => rest,
        _ => below,
    };
    if !below.iter().all(|line| line.trim().is_empty()) {
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
    } else if input.is_empty()
        || styled.zip(offset).is_some_and(|(screen, offset)| {
            // Native suggestions are wholly faint. A drawn cursor replaces only
            // the first glyph; a nonempty faint suffix must still prove the ghost.
            let mut cells = input
                .char_indices()
                .filter(|(_, c)| !c.is_whitespace())
                .peekable();
            if cells.peek().is_some_and(|(i, _)| *i == 0)
                && screen.inverse_at(offset) == Some(true)
                && screen.faint_at(offset) == Some(false)
            {
                let _ = cells.next();
            }
            cells.peek().is_some()
                && cells.all(|(i, _)| {
                    screen.faint_at(offset + i) == Some(true)
                        && screen.inverse_at(offset + i) == Some(false)
                })
        })
    {
        InputState::Empty
    } else if (input.starts_with("Try \"") && input.ends_with('"'))
        || (styled.is_some() && input == "Press up to edit queued messages")
    {
        match styled.zip(offset) {
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
    } else if styled.zip(offset).is_some_and(|(screen, offset)| {
        input
            .char_indices()
            .any(|(i, c)| c != ' ' && screen.faint_at(offset + i) != Some(false))
    }) {
        InputState::Unknown
    } else {
        InputState::Draft
    };
    // Only captured, exactly aligned notices immediately above the border are
    // chrome. The remaining transcript still decides running/idle/unknown.
    let history_end = match lines.get(..top)?.last().zip(lines.get(top)) {
        Some((line, border))
            if auto_update_failure_notice(line, border.chars().count())
                || clear_context_notice(line, border.chars().count()) =>
        {
            top.saturating_sub(1)
        }
        _ => top,
    };
    // Captured: Claude Code 2.1.327, 2026-11-22, tmux capture-pane -p -e -N.
    let survey = match (styled, lines.get(..history_end)?) {
        (
            Some(_),
            [prior @ .., "● How is Claude doing this session? (optional)", "  1: Bad    2: Fine   3: Good   0: Dismiss"],
        ) => Some(prior),
        _ => None,
    };
    let history_end = recap_start(lines.get(..history_end)?).unwrap_or(history_end);
    // Status follows the transcript, whether the input is inline or bottom-pinned.
    // Only recognized trailing chrome may be skipped, never arbitrary progress text.
    let history_start = header_end.unwrap_or(0);
    let history_rows = lines.get(history_start..history_end)?;
    let mut history: Vec<_> = history_rows
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
        [.., line] => completed_spinner(line),
    };
    let turn = if status.is_some_and(running_status) {
        TurnState::Running
    } else if idle_history
        || completed_stop_warning(lines.get(history_start..top)?, lines.get(top)?)
        || history
            .last()
            .is_some_and(|last| completed_manual_compaction(history_rows, last))
    {
        TurnState::NotRunning
    } else {
        TurnState::Unknown
    };
    // Busy chrome contradicts idle history; it cannot prove Running by itself.
    let busy_footer = footer.contains(" · esc to interrupt");
    if busy_footer && turn == TurnState::NotRunning {
        return None;
    }
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
    Some((
        facts,
        draft.filter(|_| input == InputState::Draft),
        input == InputState::Empty
            && survey.is_some_and(finished_before)
            && turn == TurnState::Unknown
            && !busy_footer,
        survey.is_some(),
    ))
}

fn clear_context_notice(line: &str, columns: usize) -> bool {
    const UPDATE_INSTALLED: &str = "✔ Update installed · /restart to apply";
    const RESTART_TO_UPDATE: &str = "✔ Update installed · Restart to update";
    if !line.starts_with("  ") || line.chars().count().checked_add(2) != Some(columns) {
        return false;
    }
    let text = line.trim_start_matches(' ');
    if matches!(text, UPDATE_INSTALLED | RESTART_TO_UPDATE) {
        return true;
    }
    // The idle hint stands beside one known update notice (s3-busy, 2.1.327).
    let Some(rest) = [UPDATE_INSTALLED, RESTART_TO_UPDATE, AUTO_UPDATE_FAILED]
        .iter()
        .find_map(|notice| text.strip_suffix(notice)?.strip_suffix(" · "))
        .unwrap_or(text)
        .strip_prefix("new task? /clear to save ")
    else {
        return false;
    };
    // Cut to fit beside the update notice at 82 columns (s3-stuckseat): a whole
    // count, then a cut " tokens" and an ellipsis. A cut count is refused.
    let Some(count) = rest.strip_suffix(" tokens").or_else(|| {
        let cut = rest.strip_suffix('…')?;
        [" token", " toke", " tok", " to", " t", " ", ""]
            .iter()
            .find_map(|tail| cut.strip_suffix(tail))
    }) else {
        return false;
    };
    let count = count.strip_suffix(['k', 'm', 'b', 't']).unwrap_or(count);
    let (whole, fraction) = count
        .split_once('.')
        .map_or((count, None), |(whole, fraction)| (whole, Some(fraction)));
    !whole.is_empty()
        && !whole.starts_with('0')
        && whole.bytes().all(|c| c.is_ascii_digit())
        && fraction.is_none_or(|f| matches!(f.as_bytes(), [b'1'..=b'9']))
}

/// Where a trailing away recap starts: Claude adds one to an idle seat left
/// unfocused, `※ recap: ` and then rows indented two cells, before blank rows
/// only (s3-stuckseat). It is chrome, not turn evidence; anything else after
/// it stays transcript.
fn recap_start(rows: &[&str]) -> Option<usize> {
    let start = rows.iter().rposition(|row| row.starts_with("※ recap: "))?;
    rows.get(start + 1..)?
        .iter()
        .skip_while(|row| row.starts_with("  ") && !row.trim().is_empty())
        .all(|row| row.trim().is_empty())
        .then_some(start)
}

const AUTO_UPDATE_FAILED: &str = "✘ Auto-update failed · Run claude doctor";

fn auto_update_failure_notice(line: &str, columns: usize) -> bool {
    line.starts_with("  ")
        && line.trim_start_matches(' ') == AUTO_UPDATE_FAILED
        && line.chars().count().checked_add(2) == Some(columns)
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

fn completed_spinner(line: &str) -> bool {
    spinner_body(line).is_some_and(|body| body.contains(" for ") && body.contains(" · done "))
}

// Captured: Claude Code 2.1.325 and 2.1.328, 2026-11-19, sealed local canned
// Messages API, tmux 3.8. Both tails need a prior column-zero done spinner;
// the action or notice is not completion evidence and never proves freshness.
fn finished_before(history: &[&str]) -> bool {
    history
        .iter()
        .rev()
        .find(|row| !row.trim().is_empty())
        .is_some_and(|row| !row.contains('…') && completed_spinner(row))
}

fn completed_stop_warning(rows: &[&str], border: &str) -> bool {
    let [before @ .., notice] = rows else {
        return false;
    };
    notice.starts_with("  ")
        && matches!(
            notice.trim_start_matches(' '),
            "Stop hook error occurred · ctrl+o to see"
                | "stop hook reported a problem · ctrl+o to see"
        )
        && notice.chars().count().checked_add(2) == Some(border.chars().count())
        && finished_before(before)
}

fn completed_manual_compaction(history: &[&str], last: &str) -> bool {
    // Use the already determined last non-chrome row, but keep the raw row
    // boundaries and indentation to verify the whole compact action.
    let Some(rows) = history
        .iter()
        .rposition(|row| row.trim() == last)
        .and_then(|end| history.get(..=end))
    else {
        return false;
    };
    let (before, command, marker) = match rows {
        [before @ .., command, marker, pre, pre_end, post, post_end]
            if completed_compact_hook(pre, pre_end, "     PreCompact [")
                && completed_compact_hook(post, post_end, "     PostCompact [") =>
        {
            (before, command, marker)
        }
        [before @ .., command, marker] => (before, command, marker),
        _ => return false,
    };
    *command == "❯ /compact"
        && matches!(
            *marker,
            "  ⎿  Compacted (ctrl+o to see full summary)"
                | "  ⎿ \u{a0}Compacted (ctrl+o to see full summary)"
        )
        && finished_before(before)
}

fn completed_compact_hook(first: &str, second: &str, prefix: &str) -> bool {
    let (Some(command), Some(rest)) = (first.strip_prefix(prefix), second.strip_prefix("     "))
    else {
        return false;
    };
    // 2.1.325 wrapped after the command bracket; 2.1.328 inside the command.
    let command_piece = |s: &str| !s.is_empty() && s.trim() == s && !s.contains(['[', ']']);
    if let Some(command) = command.strip_suffix(']') {
        command_piece(command) && rest == "completed successfully: {}"
    } else {
        command_piece(command)
            && rest
                .strip_suffix("] completed successfully: {}")
                .is_some_and(command_piece)
    }
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
mod lead_tests;

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
