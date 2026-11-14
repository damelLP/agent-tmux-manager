use atm_codex_adapter::{
    screen::{self, observe_control_screen, validate_hook_list, CONTROL_POLICY},
    CodexEventType,
};
use atm_core::harness_control::{
    ClearProof, ControlDialog, ControlKey, HookListEntry, SessionArrival,
};
use atm_core::{styled_screen::StyledScreen, FreshEditorEvidence, ScreenInput, TurnState};
macro_rules! capture {
    ($name:literal) => {
        include_str!(concat!(
            "fixtures/screen_control_p1_",
            $name,
            "_0.167.0.txt"
        ))
    };
}
macro_rules! styled {
    ($name:literal) => {
        include_str!(concat!("fixtures/screen_styled_p1_", $name, "_0.167.0.txt"))
    };
}
fn body(s: &str) -> &str {
    if s.starts_with("Captured:") {
        s.split_once('\n').map_or(s, |(_, body)| body)
    } else {
        s
    }
}
fn observe(s: &str) -> atm_core::harness_control::ControlScreen {
    observe_control_screen(ScreenInput::Styled(body(s)), None)
}
fn plain(s: &str) -> String {
    StyledScreen::parse(body(s))
        .unwrap_or_else(|error| panic!("invalid fixture styles: {error}"))
        .text()
        .to_owned()
}
const PENDING: &str = capture!("a04-review-detail-100x32");
const TRUSTED: &str = capture!("a05-trust-t-100x32");
const FRESH: &str = styled!("b14-fresh-baseline-32-100x32");

// Captured: codex-cli 0.167.0, 2026-11-14, sealed P2 private tmux.
// The manifest's first visible events come from the companion plain captures,
// independently of observe_control_screen. It includes both wraps and every
// selection, viewport, size and layout, including the missing-SubagentStop run.
const UI_EVENTS: [&str; 14] = [
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "Interrupt",
    "PreModelRequest",
    "PostModelRequest",
];

fn p2_frames() -> Vec<(String, String, usize)> {
    include_str!("fixtures/screen_control_p2_0.167.0.tsv")
        .lines()
        .map(|line| {
            let (name, first) = line
                .split_once('\t')
                .unwrap_or_else(|| panic!("invalid P2 manifest line: {line}"));
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name);
            (
                name.to_owned(),
                std::fs::read_to_string(path)
                    .unwrap_or_else(|error| panic!("missing P2 fixture {name}: {error}")),
                UI_EVENTS
                    .iter()
                    .position(|event| *event == first)
                    .unwrap_or_else(|| panic!("unknown P2 first event: {first}")),
            )
        })
        .collect()
}

#[test]
fn p2_every_real_page_and_complete_collections_preserve_p6() {
    use std::collections::BTreeMap;
    let frames = p2_frames();
    assert_eq!(frames.len(), 263);
    let mut collections: BTreeMap<String, BTreeMap<String, HookListEntry>> = BTreeMap::new();
    let mut ends: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for (name, text, first) in frames {
        let pending = name.contains("-pending-");
        let missing = name.starts_with("screen_control_p2_10-");
        let size = name.rsplit_once('-').unwrap().1;
        let key = format!("{missing}-{pending}-{size}");
        let ControlDialog::HookList {
            entries,
            above,
            below,
            trust,
            close,
        } = observe(&text).dialog
        else {
            panic!("unrecognized P2 frame: {name}");
        };
        assert_eq!((above, below), (first != 0, first + 8 != 14), "{name}");
        assert_eq!(trust, ControlKey::Character('t'));
        assert_eq!(close, ControlKey::Escape);
        let expected: Vec<_> = UI_EVENTS[first..first + 8]
            .iter()
            .filter(|event| !["Interrupt", "PreModelRequest", "PostModelRequest"].contains(event))
            .map(|event| {
                let installed = u32::from(!(missing && *event == "SubagentStop"));
                HookListEntry::new(
                    (*event).into(),
                    installed,
                    if pending { 0 } else { installed },
                    pending.then_some(installed),
                )
            })
            .collect();
        assert_eq!(entries, expected, "{name}");
        assert!(!validate_hook_list(&entries), "partial page {name}");
        let end = ends.entry(key.clone()).or_default();
        end.0 |= !above;
        end.1 |= !below;
        let collection = collections.entry(key).or_default();
        for entry in entries {
            if let Some(old) = collection.insert(entry.event.clone(), entry.clone()) {
                assert_eq!(old, entry, "conflicting overlap in {name}");
            }
        }
    }
    assert_eq!(collections.len(), 8);
    for (key, collection) in collections {
        assert_eq!(ends[&key], (true, true));
        let entries: Vec<_> = collection.into_values().collect();
        assert_eq!(entries.len(), 11);
        assert_eq!(
            validate_hook_list(&entries),
            key.starts_with("false-"),
            "{key}"
        );
        // Every proper subset, not only each eight-row viewport, must refuse.
        for mask in 0..(1usize << entries.len()) - 1 {
            let partial: Vec<_> = entries
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, row)| row.clone())
                .collect();
            assert!(!validate_hook_list(&partial), "{key}: subset {mask}");
        }
    }
}

#[test]
fn p2_synthetic_page_damage_stays_unknown() {
    // Synthetic: mutate EVERY accepted real shape, including selected extras,
    // both layouts, all widths, all marker combinations and the 10-hook run.
    for (name, text, _) in p2_frames() {
        let text = plain(&text);
        let lines: Vec<_> = text.lines().collect();
        let row_indices: Vec<_> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, line)| {
                let event = line
                    .trim()
                    .trim_start_matches("› ")
                    .split_whitespace()
                    .next()?;
                UI_EVENTS.contains(&event).then_some(i)
            })
            .collect();
        assert_eq!(row_indices.len(), 8);
        let mut mutations = vec![
            text.replace('↑', "⇧").replace('↓', "⇩"),
            text.replace(['↑', '↓'], ""), // falsely claims both ends
            text.replace('›', " "),
            text.replace(
                "Lifecycle hooks from config and enabled plugins.",
                "Lifecycle hooks",
            ),
            text.replace("esc close", "esc trust"),
            format!("{text}\nunknown footer"),
        ];
        for marker in ['↑', '↓'] {
            if text.contains(marker) {
                mutations.push(text.replace(marker, ""));
            }
        }
        // Missing/overlapping/reordered rows must not hide a gap between pages.
        for index in &row_indices {
            let mut damaged = lines.clone();
            damaged.remove(*index);
            mutations.push(damaged.join("\n"));
        }
        let mut overlap = lines.clone();
        overlap[row_indices[3]] = overlap[row_indices[2]];
        mutations.push(overlap.join("\n"));
        let mut swapped = lines.clone();
        swapped.swap(row_indices[3], row_indices[4]);
        mutations.push(swapped.join("\n"));
        let mut two_selected: Vec<_> = lines.iter().map(|line| (*line).to_owned()).collect();
        let unselected = row_indices
            .iter()
            .find(|i| !lines[**i].starts_with('›'))
            .unwrap();
        two_selected[*unselected] = format!("› {}", lines[*unselected].trim());
        mutations.push(two_selected.join("\n"));
        for damaged in mutations {
            assert_eq!(
                observe(&damaged).dialog,
                ControlDialog::Unknown,
                "{name}\n{damaged}"
            );
        }
        // Row description clipping is exact, not arbitrary prefix acceptance.
        for index in &row_indices {
            let mut damaged: Vec<_> = lines.iter().map(|line| (*line).to_owned()).collect();
            damaged[*index] = damaged[*index].trim_end().to_owned();
            damaged[*index].pop();
            assert_eq!(
                observe(&damaged.join("\n")).dialog,
                ControlDialog::Unknown,
                "{name}"
            );
        }
    }
}

#[test]
fn p2_synthetic_extra_installations_and_conflicting_collections_refuse() {
    let mut pending = Vec::new();
    let mut trusted = Vec::new();
    for (name, text, first) in p2_frames() {
        if !name.starts_with("screen_control_p2_11-") || !name.contains("100x32") {
            continue;
        }
        let ControlDialog::HookList { entries, .. } = observe(&text).dialog else {
            panic!("unrecognized {name}");
        };
        let collection = if name.contains("-pending-") {
            &mut pending
        } else {
            &mut trusted
        };
        for entry in entries {
            if !collection.contains(&entry) {
                collection.push(entry);
            }
        }
        if first != 6 {
            continue;
        }
        // Synthetic: the known non-ATM vocabulary cannot hide an installation,
        // or an unknown event, behind the omission from authorization entries.
        let text = plain(&text);
        for event in ["Interrupt", "PreModelRequest", "PostModelRequest"] {
            let row = text.lines().find(|line| line.contains(event)).unwrap();
            let installed = row.replacen("0           0", "1           1", 1);
            assert_ne!(installed, row);
            for replacement in [
                row.replacen('0', "1", 1),
                installed, // Valid installed counts still cannot authorize an extra event.
                row.replace(event, "Foreign"),
            ] {
                assert_eq!(
                    observe(&text.replace(row, &replacement)).dialog,
                    ControlDialog::Unknown
                );
            }
        }
    }
    assert!(validate_hook_list(&pending));
    assert!(validate_hook_list(&trusted));
    // Synthetic: preserve conflicting repeated rows rather than silently
    // last-write-wins deduplicating. B0's caller must compare overlaps; the
    // stateless observer cannot know what an earlier capture contained.
    let mut conflict = pending.clone();
    let mut repeated = conflict[0].clone();
    repeated.active = 1;
    repeated.review = Some(0);
    conflict.push(repeated);
    assert!(!validate_hook_list(&conflict));
    let mut overlap = pending.clone();
    overlap.push(overlap[0].clone());
    assert!(!validate_hook_list(&overlap));
    let mut mixed = pending;
    mixed[0] = trusted
        .iter()
        .find(|row| row.event == mixed[0].event)
        .unwrap()
        .clone();
    assert!(!validate_hook_list(&mixed));
}

#[test]
fn policy_reuses_existing_constants() {
    assert_eq!(CONTROL_POLICY.capture, screen::SCREEN_TEXT_FORMAT);
    assert_eq!(CONTROL_POLICY.clear_command, screen::CLEAR_COMMAND);
    assert_eq!(
        CONTROL_POLICY.session_arrival,
        SessionArrival::AtFirstPrompt
    );
    assert_eq!(
        CONTROL_POLICY.clear_proof,
        ClearProof::ConversationToFresh {
            ready_unbound_noop: true
        }
    );
    assert_eq!(CONTROL_POLICY.trust_selection_retries, 0);
}

#[test]
fn real_update_is_skippable_but_warnings_are_blocked() {
    // Captured: Codex 0.167.0, 2026-11-14, U5 private-tmux capture; sanitized.
    // U5 control-plane findings verified Escape skips this launch's update prompt;
    // P1 could not provoke it offline. No claim of a real styled update capture.
    for text in [
        include_str!("fixtures/screen_codex-startup_0.167.0.txt"),
        include_str!("fixtures/screen_codex-restart-startup_0.167.0.txt"),
    ] {
        assert_eq!(
            observe_control_screen(ScreenInput::Plain(text), None).dialog,
            ControlDialog::LaunchSkip {
                key: ControlKey::Escape
            }
        );
        // Synthetic: malformed options/selection/versions never become LaunchSkip.
        for damaged in [
            text.replace("› 1.", "1."),
            text.replace("Skip until next version", "Continue"),
            text.replace("enter continue", "enter trust"),
            text.replace("0.167.0", "bad"),
            text.replace("https://github.com", "https://elsewhere.invalid"),
        ] {
            assert_eq!(
                observe_control_screen(ScreenInput::Plain(&damaged), None).dialog,
                ControlDialog::Unknown
            );
        }
    }
    let warning = include_str!("fixtures/screen_codex-warning_0.167.0.txt");
    assert_eq!(
        observe_control_screen(ScreenInput::Plain(warning), None).dialog,
        ControlDialog::Blocked
    );
    assert_eq!(
        observe(styled!("c16-permission-visible-100x32")).dialog,
        ControlDialog::Unknown
    );
}

#[test]
fn synthetic_update_titled_warning_is_never_a_launch_skip() {
    // Synthetic: a title or transcript continuation above the real Warnings
    // modal must not turn its dismiss key into an update-skip authorization.
    let warning = include_str!("fixtures/screen_codex-warning_0.167.0.txt");
    for prefix in [
        "  Update available · 0.167.0 → 0.169.0",
        "  Update available · 1.0 → 2.0 is what the doc said",
        "  \x1b[1mUpdate available · 0.167.0 → 0.169.0\x1b[0m",
    ] {
        let text = format!("{prefix}\n{warning}");
        let decoded = plain(&text);
        for input in [ScreenInput::Plain(&decoded), ScreenInput::Styled(&text)] {
            assert_eq!(
                observe_control_screen(input, None).dialog,
                ControlDialog::Blocked,
                "{prefix}"
            );
        }
    }
}

#[test]
fn captured_worktree_folder_and_hook_review_defaults() {
    for text in [
        capture!("b27-nodaemon-new-result-100x32"),
        capture!("b31-nodaemon-clear-dialog-100x32"),
        capture!("b35-default-repo-clear-dialog-100x24"),
        capture!("b38-poor-repeat-dialog-100x24"),
    ] {
        assert_eq!(
            observe(text).dialog,
            ControlDialog::CurrentWorktree {
                selected: true,
                accept: ControlKey::Enter
            }
        );
    }
    for (text, folder) in [
        (capture!("a14-nodaemon-later-100x32"), "repo"),
        (
            capture!("a16-default-fresh-folder-trust-100x24"),
            "repo-fresh",
        ),
    ] {
        assert_eq!(
            observe(text).dialog,
            ControlDialog::FolderTrust {
                folder: format!("/tmp/xxxxxxxxxxxxxxxxxxxxxx/{folder}"),
                selected_accept: true,
                select_accept: None,
                accept: ControlKey::Enter
            }
        );
    }
    for text in [
        capture!("a02-startup-100x32"),
        capture!("a10-corrected-hooks-review-100x32"),
        capture!("a17-default-fresh-hook-trust-100x24"),
    ] {
        assert_eq!(
            observe(text).dialog,
            ControlDialog::HookReview {
                default_selected: true,
                open: ControlKey::Enter
            }
        );
    }
}

#[test]
fn real_hook_pages_are_partial_never_complete() {
    for (text, review, active) in [
        (PENDING, Some(1), 0),
        (TRUSTED, None, 1),
        (capture!("a06-hooks-command-corrected-100x32"), None, 1),
    ] {
        let ControlDialog::HookList {
            entries,
            above,
            below,
            trust,
            close,
        } = observe(text).dialog
        else {
            panic!("not a hook list");
        };
        assert_eq!(entries.len(), 8);
        assert!(!above);
        assert!(below);
        assert_eq!(trust, ControlKey::Character('t'));
        assert_eq!(close, ControlKey::Escape);
        assert_eq!(entries.first().unwrap().event, "PreToolUse");
        assert_eq!(entries.last().unwrap().event, "UserPromptSubmit");
        assert!(entries
            .iter()
            .all(|e| e.installed == 1 && e.active == active && e.review == review));
        assert!(!validate_hook_list(&entries));
    }
}

#[test]
fn synthetic_noncanonical_hook_counts_refuse_in_every_column() {
    let mut pages = vec![plain(PENDING), plain(TRUSTED)];
    for (name, text, _) in p2_frames() {
        if name.contains("11-pending-sweep-down13-120x40")
            || name.contains("11-trusted-down13-120x40")
        {
            pages.push(plain(&text));
        }
    }
    assert_eq!(pages.len(), 4);
    for text in pages {
        assert!(matches!(
            observe(&text).dialog,
            ControlDialog::HookList { .. }
        ));
        let event = if text.contains("Interrupt") {
            "Interrupt"
        } else {
            "PermissionRequest"
        };
        let row = text.lines().find(|line| line.contains(event)).unwrap();
        let tokens: Vec<_> = row.split_whitespace().collect();
        let event_index = tokens.iter().position(|token| *token == event).unwrap();
        let count_columns = if text.contains("t trust all") { 3 } else { 2 };
        for index in event_index + 1..=event_index + count_columns {
            // Synthetic: preserve the numeric value where possible but damage
            // its decimal spelling, including zero counts on omitted extra rows.
            for count in [
                format!("+{}", tokens[index]),
                format!("0{}", tokens[index]),
                "-0".into(),
                "١".into(),
                "4294967296".into(),
            ] {
                let mut damaged = tokens.clone();
                damaged[index] = &count;
                let mutated = text.replace(row, &damaged.join(" "));
                for input in [ScreenInput::Plain(&mutated), ScreenInput::Styled(&mutated)] {
                    assert_eq!(
                        observe_control_screen(input, None).dialog,
                        ControlDialog::Unknown,
                        "{event} column {index}: {count}"
                    );
                }
            }
        }
    }
}

#[test]
fn synthetic_complete_sets_follow_p6_exactly() {
    // Synthetic: validator data exercise mixed active counts, not an invented
    // accepted screen. Real complete collections are covered by the P2 replay.
    let pending: Vec<_> = CodexEventType::hook_names()
        .enumerate()
        .map(|(i, name)| {
            HookListEntry::new(name.into(), 1, (i % 2) as u32, Some(1 - (i % 2) as u32))
        })
        .collect();
    let trusted: Vec<_> = CodexEventType::hook_names()
        .map(|name| HookListEntry::new(name.into(), 1, 1, None))
        .collect();
    assert!(validate_hook_list(&pending));
    assert!(validate_hook_list(&trusted));
    let mut reordered = pending.clone();
    reordered.reverse();
    assert!(validate_hook_list(&reordered));
    assert!(!validate_hook_list(&[]));
    assert!(!validate_hook_list(&pending[..10]));
    for field in 0..7 {
        let mut rows = pending.clone();
        match field {
            0 => rows.push(rows[0].clone()),
            1 => rows[0].event = "Foreign".into(),
            2 => rows[0].installed = 2,
            3 => rows[0].active = 2,
            4 => rows[0].review = Some(2),
            5 => rows[0].review = None,
            _ => rows[0].installed = u32::MAX,
        }
        assert!(!validate_hook_list(&rows));
    }
    let mut inactive = trusted;
    inactive[0].active = 0;
    assert!(!validate_hook_list(&inactive));
}

#[test]
fn synthetic_malformed_dialogs_refuse_and_history_does_not_act() {
    for text in [
        capture!("a16-default-fresh-folder-trust-100x24"),
        capture!("a17-default-fresh-hook-trust-100x24"),
        capture!("b31-nodaemon-clear-dialog-100x32"),
    ] {
        let text = plain(text);
        for damaged in [
            text.replace("› 1.", "1."),
            text.replace("  2.", "› 2."),
            text.replace("enter ", "unknown "),
            format!("{text}\nunknown footer"),
            format!("{text}\x1b[8m"),
            format!("{text}\x1b["),
        ] {
            assert_eq!(observe(&damaged).dialog, ControlDialog::Unknown);
        }
        assert_eq!(
            observe(&format!("{text}\n{}", body(FRESH))).dialog,
            ControlDialog::None
        );
        assert_eq!(
            observe(&format!("{text}\nshell$ ")).dialog,
            ControlDialog::Unknown
        );
    }
    let folder = plain(capture!("a16-default-fresh-folder-trust-100x24"));
    for damaged in [
        folder.replace("/repo-fresh", "/..."),
        folder.replace("/repo-fresh", "/long\n wrapped"),
        folder.replace("Trust this folder?", "Trust?"),
    ] {
        assert_eq!(observe(&damaged).dialog, ControlDialog::Unknown);
    }
}

#[test]
fn synthetic_hook_table_damage_never_creates_complete_or_actionable_list() {
    let pending = plain(PENDING);
    let trusted = plain(TRUSTED);
    for damaged in [
        pending.replace("Review      Description", "Description"),
        pending.replace("t trust all", "t trust other"),
        pending.replace("need review before", "changed before"),
        pending.replace("PreToolUse", "Foreign"),
        pending.replace("PermissionRequest", "PreToolUse"),
        pending.replace("1           0           1", "2           0           1"),
        pending.replace("1           0           1", "1           2           1"),
        pending.replace("1           0           1", "4294967296  0           1"),
        pending.replace('↓', ""),
        pending.replace('↓', "↑"),
        trusted.replace("enter details", "enter review"),
        trusted.replace(
            "  Issues",
            "  ⚠ 11 hooks need review before they can run.\n  Issues",
        ),
        format!("{PENDING}\nunknown footer"),
    ] {
        assert_eq!(
            observe(&damaged).dialog,
            ControlDialog::Unknown,
            "{damaged}"
        );
    }
}

#[test]
fn conversation_evidence_does_not_change_worked_line_parser() {
    let completed = capture!("b12-completed-before-new-100x32");
    for text in [
        completed,
        styled!("c07-working-held-100x24"),
        styled!("c02-multiline-100x24"),
    ] {
        assert!(observe(text).conversation);
        assert_eq!(observe(text).dialog, ControlDialog::None);
        assert_eq!(
            observe(text).facts.fresh_editor,
            FreshEditorEvidence::Unknown
        );
    }
    assert_eq!(observe(completed).facts.turn, TurnState::Unknown); // U8g owns this.
    for text in [FRESH, styled!("b37-poor-baseline-100x24")] {
        assert!(!observe(text).conversation);
        assert_eq!(
            observe(text).facts.fresh_editor,
            FreshEditorEvidence::Present
        );
    }
    // Synthetic: composer text is not history; unknown active layout stays Unknown.
    let draft = plain(FRESH).replace("Ask Codex to do anything", "fake conversation");
    assert!(!observe(&draft).conversation);
    assert_eq!(
        observe(&format!("{FRESH}\nunknown footer")).dialog,
        ControlDialog::Unknown
    );
}
