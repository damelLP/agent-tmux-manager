//! Captured: codex-cli 0.167.0, 2026-11-14, U5 and sealed P1 private tmux.
//! See fixture README. Only captured default dialog selections and partial top
//! hook pages are recognized. Full/lower pages need real evidence. Hook counts
//! cannot identify commands: same-event/count replacement remains undetectable.
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
        && entries.iter().all(|e| valid_counts(e))
        && (entries.iter().all(|e| e.review.is_some())
            || entries.iter().all(|e| e.review.is_none()))
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
    let text = match input {
        ScreenInput::Plain(text) if cursor.is_none() => text,
        ScreenInput::Styled(text) => {
            let Ok(screen) = StyledScreen::parse(text) else {
                return result;
            };
            decoded = screen;
            decoded.text()
        }
        _ => return result,
    };
    if text.chars().any(|c| c.is_control() && c != '\n') {
        return result;
    }
    let lines: Vec<_> = text.lines().map(|s| s.trim_end_matches(' ')).collect();
    let body = nonblank(&lines);
    result.facts = observe_screen(input, cursor);
    if known_dialog(&lines) {
        result.dialog = if body
            .first()
            .is_some_and(|s| s.starts_with("Update available · "))
        {
            ControlDialog::LaunchSkip {
                key: ControlKey::Escape,
            }
        } else {
            ControlDialog::Blocked
        };
    } else if let Some(dialog) = start_dialog(&body).or_else(|| hook_list(&body)) {
        result.dialog = dialog;
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

fn hook_list(body: &[&str]) -> Option<ControlDialog> {
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
    // Only the captured top-page down indicator is evidence-backed. No inferred
    // completion from its absence, and no guessed lower-page/up-marker grammar.
    let ("↓", rows) = tail.split_last().map(|(marker, rows)| (*marker, rows))? else {
        return None;
    };
    let mut entries = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let row = if i == 0 {
            row.strip_prefix("› ")?
        } else {
            row
        };
        let mut columns = row.split_whitespace();
        let event = columns.next()?;
        let installed = columns.next()?.parse().ok()?;
        let active = columns.next()?.parse().ok()?;
        let review = if pending {
            Some(columns.next()?.parse().ok()?)
        } else {
            None
        };
        let entry = HookListEntry::new(event.into(), installed, active, review);
        if !valid_counts(&entry)
            || !CodexEventType::hook_names().any(|name| name == event)
            || entries.iter().any(|old: &HookListEntry| old.event == event)
            || description(event)? != columns.collect::<Vec<_>>().join(" ")
        {
            return None;
        }
        entries.push(entry);
    }
    if entries.is_empty() || entries.len() >= CodexEventType::hook_names().count() {
        return None;
    }
    Some(ControlDialog::HookList {
        entries,
        above: false,
        below: true,
        trust: ControlKey::Character('t'),
        close: ControlKey::Escape,
    })
}

fn description(event: &str) -> Option<&'static str> {
    match event {
        "PreToolUse" => Some("Before a tool executes"),
        "PermissionRequest" => Some("When permission is requested"),
        "PostToolUse" => Some("After a tool executes"),
        "PreCompact" => Some("Before context compaction"),
        "PostCompact" => Some("After context compaction"),
        "SessionStart" => Some("When a new session starts"),
        "SessionEnd" => Some("Right before a session ends"),
        "UserPromptSubmit" => Some("When the user submits a prompt"),
        _ => None,
    }
}
