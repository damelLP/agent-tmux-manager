//! Lifecycle observations extend, but do not change, the legacy observer.
//!
//! Captured: Claude Code 2.1.325, 2026-11-14, sealed P1 private tmux.
//! See the fixture README. Trust paths are facts, never authorization. Prefixes
//! above the first complete header are uninterpreted shell output, except that
//! Claude editor/transcript chrome there vetoes freshness. A second header is
//! history, not a reason to discard an existing conversation.
//! Column-0 `❯ ` and `● ` lines above the header veto freshness, including
//! shell prompts and service-status output.
use super::*;
use atm_core::harness_control::{
    ClearProof, ControlDialog, ControlKey, ControlPolicy, ControlScreen, SessionArrival,
};

const MODE_FOOTERS: [&str; 4] = [
    "⏵⏵ auto mode on (shift+tab to cycle)",
    "⏵⏵ auto mode on (shift+tab to cycle) · ← for agents",
    "⏸ manual mode on",
    "⏸ manual mode on · ← for agents",
];

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
    match styled.map(completion_draft) {
        Some(CompletionDraft::Recognised { .. }) => {
            result.dialog = ControlDialog::None;
            return result;
        }
        Some(CompletionDraft::Invalid) => return result,
        _ => {}
    }
    let header = lines.iter().enumerate().find_map(|(i, line)| {
        line.starts_with(" ▐▛███▛█   Claude Code v")
            .then(|| lines.get(i..).and_then(header_end).map(|end| (i, i + end)))
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
    if !MODE_FOOTERS.contains(&footer) {
        return result;
    }
    // Without a complete header, only already-recognized ordinary facts apply.
    if let Some((start, end)) = header {
        if let Some((facts, _)) = editor_facts(&lines, styled, Some(end), before_footer) {
            result.facts = facts;
        }
        if lines.get(..start).unwrap_or_default().iter().any(|line| {
            let trimmed = line.trim();
            line.starts_with("❯ ")
                || line.starts_with("❯\u{a0}")
                || line.starts_with("● ")
                || MODE_FOOTERS
                    .contains(&trimmed.split_once("  ").map_or(trimmed, |(left, _)| left))
        }) {
            result.facts.fresh_editor = FreshEditorEvidence::Unknown;
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
                let history = lines
                    .get(header.map_or(0, |(_, end)| end)..top)
                    .unwrap_or_default();
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompletionDraft<'a> {
    Absent,
    Recognised { draft: &'a str, selected: &'a str },
    Invalid,
}

/// Captured completion layouts: above the editor with optional other matches,
/// or the older single item below it. Only Absent permits ordinary fallback.
/// Description wording is not command identity; the selected name must be whole.
pub(super) fn completion_draft(screen: &StyledScreen) -> CompletionDraft<'_> {
    parse_completion_draft(screen).unwrap_or(CompletionDraft::Invalid)
}

fn parse_completion_draft(screen: &StyledScreen) -> Option<CompletionDraft<'_>> {
    let mut lines: Vec<_> = screen
        .text()
        .lines()
        .map(|s| s.trim_end_matches(' '))
        .collect();
    while lines.last().is_some_and(|s| s.is_empty()) {
        lines.pop();
    }
    let Some(lower) = lines.iter().rposition(|s| rule(s, '─')) else {
        return Some(CompletionDraft::Absent);
    };
    let before = lines.get(..lower)?;
    let Some(top) = before.iter().rposition(|s| rule(s, '─')) else {
        return Some(CompletionDraft::Absent);
    };
    let history = lines.get(..top)?;
    let below: Vec<_> = lines
        .get(lower + 1..)?
        .iter()
        .copied()
        .filter(|s| !s.is_empty())
        .collect();
    // Look only at the adjacent block. A damaged selector/margin must still veto
    // fallback when a command/description row identifies a completion candidate.
    let has_menu = |rows: &[&str]| {
        rows.iter().any(|s| {
            let row = s.trim_start_matches(' ');
            (s.starts_with(' ') && row.starts_with("❯ "))
                || row.split_once("  ").is_some_and(|(label, _)| {
                    label.starts_with('/') || label.starts_with('…') || label.contains(" /")
                })
        })
    };
    let mut above = &[][..];
    let mut end = history.len();
    while end > 0 {
        let prefix = history.get(..end)?;
        let Some(last) = prefix.iter().rposition(|s| !s.is_empty()) else {
            break;
        };
        let prefix = prefix.get(..last + 1)?;
        let start = prefix
            .iter()
            .rposition(|s| s.is_empty())
            .map_or(0, |i| i + 1);
        let block = prefix.get(start..)?;
        if has_menu(block) {
            // Retain gaps: an interrupted menu is invalid, not an absent popup.
            above = history.get(start..)?;
            break;
        }
        if !block.iter().all(|s| s.starts_with("     ")) {
            break;
        }
        end = start;
    }
    if above.is_empty() && end < history.len() && history.get(..end)?.iter().all(|s| s.is_empty()) {
        // Continuations reached the pane top without a command/selector row.
        return None;
    }
    let ordinary = editor(&lines, Some(screen));
    let (menu, above_editor) = if !above.is_empty() {
        (above, true)
    } else if ordinary.is_some()
        || below.last().is_some_and(|footer| {
            let footer = footer.trim();
            MODE_FOOTERS.contains(&footer.split_once("  ").map_or(footer, |(left, _)| left))
        })
    {
        // A recognized mode footer owns its opaque status slot, including in
        // manual mode. The ordinary/control readers still validate the frame.
        return Some(CompletionDraft::Absent);
    } else if has_menu(&below) {
        (below.as_slice(), false)
    } else {
        return Some(CompletionDraft::Absent);
    };
    if lines.get(top) != lines.get(lower)
        || (above_editor
            && !ordinary
                .as_ref()
                .is_some_and(|(facts, _)| facts.input == InputState::Draft))
    {
        return None;
    }
    let [row] = lines.get(top + 1..lower)? else {
        return None;
    };
    let draft = row.strip_prefix("❯\u{a0}").filter(|s| !s.is_empty())?;
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
    let mut selected = None;
    let mut commands = Vec::new();
    let mut description_start = None;
    for row in menu {
        let entry = row.strip_prefix("  ❯ ").map(|s| (s, true)).or_else(|| {
            above_editor
                .then(|| row.strip_prefix("    "))
                .flatten()
                .filter(|s| s.starts_with('/') || s.starts_with('…'))
                .map(|s| (s, false))
        });
        if let Some((label, is_selected)) = entry {
            let (command, description) = label.split_once("  ")?;
            if command.chars().any(char::is_whitespace) || description.trim().is_empty() {
                return None;
            }
            let column = row.chars().count() - description.trim_start().chars().count();
            if description_start.is_some_and(|start| start != column) {
                return None;
            }
            description_start = Some(column);
            if selected == Some(command) || (is_selected && commands.contains(&command)) {
                return None;
            }
            commands.push(command);
            if is_selected
                && (!command.strip_prefix('/').is_some_and(|name| {
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                }) || selected.replace(command).is_some())
            {
                return None;
            }
        } else if description_start != Some(row.len() - row.trim_start_matches(' ').len())
            || row.trim().is_empty()
            || row.contains('❯')
        {
            return None;
        }
    }
    Some(CompletionDraft::Recognised {
        draft,
        selected: selected?,
    })
}

fn trust_dialog(lines: &[&str]) -> Option<ControlDialog> {
    let body: Vec<_> = lines
        .iter()
        .enumerate()
        .map(|(i, s)| (i, s.trim()))
        .filter(|(_, s)| !s.is_empty())
        .collect();
    let [.., (no_index, no), (yes_index, yes), (_, _footer)] = body.as_slice() else {
        return None;
    };
    let selected = match (*no, *yes) {
        ("❯ No, exit", "Yes, I trust this folder") => false,
        ("No, exit", "❯ Yes, I trust this folder") => true,
        _ => return None,
    };
    // Validate the full chrome/path through the existing trust grammar. Only
    // normalize the already-verified pair of selection markers, not any prose.
    let mut canonical = lines.to_vec();
    *canonical.get_mut(*no_index)? = "❯ No, exit";
    *canonical.get_mut(*yes_index)? = "Yes, I trust this folder";
    let DialogState::FolderTrust { folder } = dialog(&canonical)? else {
        return None;
    };
    Some(ControlDialog::FolderTrust {
        folder,
        selected_accept: selected,
        select_accept: (!selected).then_some(ControlKey::Down),
        accept: ControlKey::Enter,
    })
}
