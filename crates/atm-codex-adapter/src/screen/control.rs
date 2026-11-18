//! Captured: codex-cli 0.167.0, 2026-11-14, U5 and sealed P1/P2 private tmux.
//! See fixture README. Hook pages include selection movement, both scroll edges
//! and zero-count vocabulary rows. Only ATM rows are returned for P6 validation;
//! known uninstalled extras are checked, never silently discarded if installed.
//! Counts cannot identify commands: same-event/count replacement is undetectable.
//! Captured: codex-cli 0.167.0, 2026-11-14, P5 sealed private tmux 3.8
//! capture-pane -p -e -N, 80/100/120 columns. Only the exact sole inverse-selected
//! `/new` completion is non-modal for control; delivery facts remain Other.
//! Captured: codex-cli 0.167.0, 2026-11-15, P6 sealed private tmux 3.8
//! capture-pane -p -e -N, 80x24/100x32/120x40, full and short transcripts.
//! The upper boundary is blank or a faint, non-inverse finished-turn status
//! after a blank. A nearby candidate across that separator remains ambiguous.
use super::*;
use crate::CodexEventType;
use atm_core::harness_control::{
    ClearProof, ControlDialog, ControlKey, ControlPolicy, ControlScreen, HookListEntry,
    SessionArrival,
};

pub const CONTROL_POLICY: ControlPolicy = ControlPolicy::new(
    SCREEN_TEXT_FORMAT,
    SessionArrival::AtFirstPrompt,
    CLEAR_COMMAND,
    ClearProof::ConversationToFresh {
        ready_unbound_noop: true,
    },
    0,
);

/// Validate a complete collected set, not a page or its provenance. The caller
/// must have parsed exact layout/footer evidence and accounted for both ends
/// and consistent overlaps before using collected rows to authorize anything.
#[must_use]
pub fn validate_hook_list(entries: &[HookListEntry]) -> bool {
    let names: Vec<_> = CodexEventType::hook_names().collect();
    entries.len() == names.len()
        && names
            .iter()
            .all(|name| entries.iter().filter(|e| e.event == *name).count() == 1)
        && entries.iter().all(valid_counts)
        && (entries.iter().all(|e| e.review.is_some())
            || entries.iter().all(|e| e.review.is_none()))
}

fn hook_count(value: &str) -> Option<u32> {
    if value.is_empty()
        || !value.bytes().all(|c| c.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok()
}

fn valid_counts(entry: &HookListEntry) -> bool {
    entry.installed == 1
        && match entry.review {
            Some(review) => entry.active <= 1 && review == 1 - entry.active,
            None => entry.active == 1,
        }
}

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
            #[cfg(test)]
            DECODE_COUNT.with(|count| count.set(count.get() + 1));
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
    let lines: Vec<_> = text.lines().map(|s| s.trim_end_matches(' ')).collect();
    let body = nonblank(&lines);
    let (facts, draft) = inspect_prepared(text, styled);
    result.facts = facts;
    if known_dialog(&lines) {
        result.dialog = if update_prompt(&body) {
            ControlDialog::LaunchSkip {
                key: ControlKey::Escape,
            }
        } else {
            ControlDialog::Blocked
        };
    } else if let Some(dialog) = start_dialog(&body).or_else(|| hook_list(&body, text)) {
        result.dialog = dialog;
    } else if result.facts.dialog == DialogState::Other
        && draft_matches(draft.as_deref(), CLEAR_COMMAND) == Some(true)
        && styled.is_some_and(clear_completion)
    {
        result.dialog = ControlDialog::None;
    } else if result.facts.dialog == DialogState::None {
        result.dialog = ControlDialog::None;
        // Composer comes after the last blank separator and before model/footer.
        // Find its last occurrence; earlier prompts belong to the transcript.
        if let Some(composer) = lines.iter().rposition(|s| s.starts_with("› ")) {
            let history = lines.get(..composer).unwrap_or_default();
            result.conversation = history.iter().any(|s| s.starts_with("› "))
                && history.iter().any(|s| s.starts_with("• "));
        }
    }
    result
}

fn clear_completion(screen: &StyledScreen) -> bool {
    let lines: Vec<_> = screen
        .text()
        .lines()
        .map(|s| s.trim_end_matches(' '))
        .collect();
    let Some(composer) = lines.iter().rposition(|s| *s == "› /new") else {
        return false;
    };
    let Some([history @ .., upper, menu, lower]) = lines.get(..composer) else {
        return false;
    };
    if !lower.is_empty() || *menu != "› /new  start a new chat during a conversation" {
        return false;
    }
    if !upper.is_empty() {
        let offset = screen
            .text()
            .split_inclusive('\n')
            .take(history.len())
            .map(str::len)
            .sum::<usize>();
        if history.last() != Some(&"")
            || !upper.strip_prefix("  ").is_some_and(worked_status)
            || !upper.char_indices().all(|(i, _)| {
                screen.faint_at(offset + i) == Some(true)
                    && screen.inverse_at(offset + i) == Some(false)
            })
        {
            return false;
        }
    }
    // Do not mistake a candidate separated by blanks for transcript history.
    if history
        .iter()
        .rev()
        .find(|line| !line.is_empty())
        .is_some_and(|line| completion_candidate(line))
    {
        return false;
    }
    // The menu precedes the blank separator and composer. Offsets are decoded
    // UTF-8 bytes, not columns; retain original padding when counting rows.
    let offset = screen
        .text()
        .split_inclusive('\n')
        .take(composer.saturating_sub(2))
        .map(str::len)
        .sum::<usize>();
    menu.char_indices()
        .all(|(i, _)| screen.inverse_at(offset + i) == Some(true))
}

fn completion_candidate(line: &str) -> bool {
    let line = line.trim_start_matches(' ');
    let line = line.strip_prefix("› ").unwrap_or(line);
    line.split_once("  ").is_some_and(|(command, description)| {
        command
            .strip_prefix('/')
            .is_some_and(|name| !name.is_empty() && !name.chars().any(char::is_whitespace))
            && !description.trim_matches(' ').is_empty()
    })
}

fn full_folder(folder: &str) -> bool {
    folder.starts_with('/') && !folder.contains(['…', '⤶']) && !folder.contains("...")
}

fn start_dialog(body: &[&str]) -> Option<ControlDialog> {
    if let Some(start) = body
        .iter()
        .rposition(|s| *s == "Where should the new conversation run?")
    {
        if let ["Where should the new conversation run?", current, create, "enter select · esc back"] =
            body.get(start..)?
        {
            if current.split_whitespace().collect::<Vec<_>>().join(" ") == "› 1. Use current Git worktree Keep using the current working directory"
                && create.split_whitespace().collect::<Vec<_>>().join(" ") == "2. Create new Git worktree Create a separate checkout of this repository in another directory" {
                return Some(ControlDialog::CurrentWorktree { selected: true, accept: ControlKey::Enter });
            }
        }
    }
    match body {
        ["Folder access", folder, paragraph @ .., "› 1. Trust and continue", "2. Back to Agent Command Center", "enter continue · esc back"]
            if full_folder(folder) && paragraph.join(" ") == "Trust this folder? Codex can read, edit, and run files here, subject to your permission settings. Folder settings can run code automatically, even without a model request. Continue only if you trust these files. Your trust decision will be saved." => {
            Some(ControlDialog::FolderTrust { folder: (*folder).into(), selected_accept: true, select_accept: None, accept: ControlKey::Enter })
        }
        ["Hooks need review", count, "Hooks can run outside the sandbox after you trust them.", "› 1. Review hooks", "2. Trust all and continue", "3. Continue without trusting (hooks won't run)", "enter confirm · esc skip"]
            if count.strip_suffix(" hooks are new or changed.").is_some_and(positive_integer) => {
            Some(ControlDialog::HookReview { default_selected: true, open: ControlKey::Enter })
        }
        _ => None,
    }
}

fn hook_list(body: &[&str], text: &str) -> Option<ControlDialog> {
    let start = body.iter().rposition(|s| *s == "Hooks")?;
    let ["Hooks", "Lifecycle hooks from config and enabled plugins.", tail @ ..] =
        body.get(start..)?
    else {
        return None;
    };
    let (pending, tail) = match tail.split_first() {
        Some((banner, rest))
            if banner
                .strip_prefix("⚠ ")
                .and_then(|s| s.strip_suffix(" hooks need review before they can run."))
                .is_some_and(positive_integer) =>
        {
            (true, rest)
        }
        _ => (false, tail),
    };
    let tail = match tail {
        ["Issues", notice, rest @ ..]
            if notice
                .strip_prefix("⚠ clamping SessionEnd hook timeout to ")
                .and_then(|s| s.split_once("s in "))
                .is_some_and(|(seconds, path)| positive_integer(seconds) && full_folder(path)) =>
        {
            rest
        }
        _ => tail,
    };
    let (columns, tail) = tail.split_first()?;
    // Header padding is tmux allocation, not the viewport edge. Clip from the
    // header's description column to the widest captured line, including spaces.
    let header = text.lines().rev().find(|line| line.trim() == *columns)?;
    let start = header.get(..header.find("Description")?)?.chars().count();
    let clip = text
        .lines()
        .map(|line| line.chars().count())
        .max()?
        .checked_sub(start)?;
    let columns = columns.split_whitespace().collect::<Vec<_>>().join(" ");
    if columns
        != if pending {
            "Event Installed Active Review Description"
        } else {
            "Event Installed Active Description"
        }
    {
        return None;
    }
    let (footer, tail) = tail.split_last()?;
    if *footer
        != if pending {
            "t trust all · enter review · esc close"
        } else {
            "enter details · esc close"
        }
    {
        return None;
    }
    let above = tail.first() == Some(&"↑");
    let rows = if above { tail.get(1..)? } else { tail };
    let below = rows.last() == Some(&"↓");
    let rows = if below {
        rows.get(..rows.len().checked_sub(1)?)?
    } else {
        rows
    };
    // All P1/P2 viewports have eight rows, independent of terminal height.
    // Missing markers must not turn a damaged page into evidence of an end.
    if rows.len() != 8 {
        return None;
    }
    let mut entries = Vec::new();
    let mut first = None;
    let mut selections = 0;
    for (offset, row) in rows.iter().enumerate() {
        let row = if let Some(row) = row.strip_prefix("› ") {
            selections += 1;
            row
        } else {
            row
        };
        let mut columns = row.split_whitespace();
        let event = columns.next()?;
        let installed = hook_count(columns.next()?)?;
        let active = hook_count(columns.next()?)?;
        let review = if pending {
            Some(hook_count(columns.next()?)?)
        } else {
            None
        };
        let entry = HookListEntry::new(event.into(), installed, active, review);
        let position = HOOK_ROWS.iter().position(|(name, _)| *name == event)?;
        let first = *first.get_or_insert(position);
        let expected: String = HOOK_ROWS.get(position)?.1.chars().take(clip).collect();
        let description = columns.collect::<Vec<_>>().join(" ");
        let required = CodexEventType::hook_names().any(|name| name == event);
        let uninstalled = entry.installed == 0
            && entry.active == 0
            && entry.review.is_none_or(|review| review == 0);
        if position != first + offset
            || !(uninstalled || (required && valid_counts(&entry)))
            || expected.trim_end() != description
        {
            return None;
        }
        if required {
            entries.push(entry);
        }
    }
    let first = first?;
    if selections != 1 || above != (first != 0) || below != (first + rows.len() < HOOK_ROWS.len()) {
        return None;
    }
    Some(ControlDialog::HookList {
        entries,
        above,
        below,
        trust: ControlKey::Character('t'),
        close: ControlKey::Escape,
    })
}

// UI order is distinct from registration order. The final three events are
// vocabulary only, not ATM subscriptions; their counts must all remain zero.
const HOOK_ROWS: [(&str, &str); 14] = [
    ("PreToolUse", "Before a tool executes"),
    ("PermissionRequest", "When permission is requested"),
    ("PostToolUse", "After a tool executes"),
    ("PreCompact", "Before context compaction"),
    ("PostCompact", "After context compaction"),
    ("SessionStart", "When a new session starts"),
    ("SessionEnd", "Right before a session ends"),
    ("UserPromptSubmit", "When the user submits a prompt"),
    ("SubagentStart", "When a subagent is created"),
    ("SubagentStop", "Right before a subagent ends its turn"),
    ("Stop", "Right before Codex ends its turn"),
    ("Interrupt", "Right before an interrupted turn is aborted"),
    ("PreModelRequest", "Before a model request"),
    (
        "PostModelRequest",
        "After a model request, including failures",
    ),
];

#[cfg(test)]
mod decode_tests;
