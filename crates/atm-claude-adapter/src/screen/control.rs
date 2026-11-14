//! Lifecycle observations extend, but do not change, the legacy observer.
//!
//! Captured: Claude Code 2.1.325, 2026-11-14, sealed P1 private tmux.
//! See the fixture README. Trust paths are facts, never authorization. Prefixes
//! above the first complete header are uninterpreted shell output; a second
//! header is history, not a reason to discard an existing conversation.
use super::*;
use atm_core::harness_control::{
    ClearProof, ControlDialog, ControlKey, ControlPolicy, ControlScreen, SessionArrival,
};

pub const CONTROL_POLICY: ControlPolicy = ControlPolicy::new(
    SCREEN_TEXT_FORMAT,
    SessionArrival::AtStart,
    CLEAR_COMMAND,
    ClearProof::ChangedSession {
        start_source: Some("clear"),
    },
    1,
);

#[must_use]
pub fn observe_control_screen(
    input: ScreenInput<'_>,
    cursor: Option<ScreenCursor>,
) -> ControlScreen {
    let mut result = ControlScreen::default();
    let decoded;
    let (text, styled) = match input {
        ScreenInput::Plain(text) if cursor.is_none() => (text, None),
        ScreenInput::Styled(text) => {
            let Ok(screen) = StyledScreen::parse(text) else {
                return result;
            };
            decoded = screen;
            (decoded.text(), Some(&decoded))
        }
        _ => return result,
    };
    if text.chars().any(|c| c.is_control() && c != '\n') {
        return result;
    }
    let mut lines: Vec<_> = text.lines().map(|s| s.trim_end_matches(' ')).collect();
    while lines.last().is_some_and(|s| s.is_empty()) {
        lines.pop();
    }
    result.facts = observe_screen(input, cursor);
    if let Some(trust) = trust_dialog(&lines) {
        result.dialog = trust;
        return result;
    }
    if result.facts.dialog == DialogState::Permission {
        result.dialog = ControlDialog::Blocked;
        return result;
    }
    if styled.and_then(completion_draft).is_some() {
        result.dialog = ControlDialog::None;
        return result;
    }
    let header = lines.iter().enumerate().find_map(|(i, line)| {
        line.starts_with(" ▐▛███▛█   Claude Code v")
            .then(|| lines.get(i..).and_then(header_end).map(|end| i + end))
            .flatten()
    });
    let Some((footer, before_footer)) = lines.split_last() else {
        return result;
    };
    let (footer, before_footer) = if footer.trim() == "Ctrl+Y to paste deleted text" {
        let Some(parts) = before_footer.split_last() else {
            return result;
        };
        parts
    } else {
        (footer, before_footer)
    };
    let footer = footer
        .trim()
        .split_once("  ")
        .map_or(footer.trim(), |(left, _)| left);
    if !matches!(
        footer,
        "⏵⏵ auto mode on (shift+tab to cycle)"
            | "⏵⏵ auto mode on (shift+tab to cycle) · ← for agents"
            | "⏸ manual mode on"
            | "⏸ manual mode on · ← for agents"
    ) {
        return result;
    }
    // Without a complete header, only already-recognized ordinary facts apply.
    if let Some(end) = header {
        if let Some((facts, _)) = editor_facts(&lines, styled, Some(end), before_footer) {
            result.facts = facts;
        }
    }
    if result.facts.dialog == DialogState::None
        && (header.is_some() || result.facts.turn != TurnState::Unknown)
    {
        result.dialog = ControlDialog::None;
        if let Some(lower) = before_footer.iter().rposition(|s| rule(s, '─')) {
            if let Some(top) = lines
                .get(..lower)
                .and_then(|s| s.iter().rposition(|s| rule(s, '─')))
            {
                let history = lines.get(header.unwrap_or(0)..top).unwrap_or_default();
                result.conversation = history.iter().any(|s| {
                    s.trim()
                        .strip_prefix("❯ ")
                        .is_some_and(|s| !s.is_empty() && s != "/clear")
                }) && history.iter().any(|s| s.trim().starts_with("● "));
            }
        }
    }
    result
}

/// The one captured completion layout: a single selected command and its
/// possibly wrapped description. Description wording is not command identity.
pub(super) fn completion_draft(screen: &StyledScreen) -> Option<(&str, &str)> {
    let lines: Vec<_> = screen
        .text()
        .lines()
        .map(|s| s.trim_end_matches(' '))
        .collect();
    let lower = lines.iter().rposition(|s| rule(s, '─'))?;
    let top = lines.get(..lower)?.iter().rposition(|s| rule(s, '─'))?;
    if lines.get(top) != lines.get(lower) {
        return None;
    }
    let [row] = lines.get(top + 1..lower)? else {
        return None;
    };
    let draft = row.strip_prefix("❯\u{a0}")?;
    if draft.is_empty() {
        return None;
    }
    let offset = screen
        .text()
        .split_inclusive('\n')
        .take(top + 1)
        .map(str::len)
        .sum::<usize>()
        + "❯\u{a0}".len();
    if draft
        .char_indices()
        .any(|(i, c)| !c.is_whitespace() && screen.faint_at(offset + i) != Some(false))
    {
        return None;
    }
    let menu: Vec<_> = lines
        .get(lower + 1..)?
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect();
    let (selected, continuation) = menu.split_first()?;
    let name_and_description = selected.strip_prefix("  ❯ ")?;
    let (command, description) = name_and_description.split_once("  ")?;
    if !command.strip_prefix('/').is_some_and(|name| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    }) || description.trim().is_empty()
    {
        return None;
    }
    let description_start = selected.chars().count() - description.trim_start().chars().count();
    if !continuation.iter().all(|s| {
        s.len() - s.trim_start_matches(' ').len() == description_start
            && !s.trim().is_empty()
            && !s.contains('❯')
    }) {
        return None;
    }
    Some((draft, command))
}

fn trust_dialog(lines: &[&str]) -> Option<ControlDialog> {
    let top = lines.iter().rposition(|s| rule(s, '─'))?;
    let body: Vec<_> = lines
        .get(top + 1..)?
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let ["Accessing workspace:", folder, paragraph @ .., "Claude Code'll be able to read, edit, and execute files here.", "Security guide", no, yes, "Enter to confirm · Esc to cancel"] =
        body.as_slice()
    else {
        return None;
    };
    if paragraph.join(" ") != "Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team). If not, take a moment to review what's in this folder first."
        || !folder.starts_with('/') || folder.contains(['…', '⤶']) || folder.contains("...") { return None; }
    let selected = match (*no, *yes) {
        ("❯ No, exit", "Yes, I trust this folder") => false,
        ("No, exit", "❯ Yes, I trust this folder") => true,
        _ => return None,
    };
    Some(ControlDialog::FolderTrust {
        folder: (*folder).into(),
        selected_accept: selected,
        select_accept: (!selected).then_some(ControlKey::Down),
        accept: ControlKey::Enter,
    })
}
