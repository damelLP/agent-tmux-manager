//! Captured: Claude Code 2.1.325, 2026-11-15, read-only tmux styled captures.
//! Fixtures retain the active chrome; transcript and identifying status values are redacted.

use super::*;
use atm_core::harness_control::{ControlDialog, ControlScreen};

const IDLE: &str = include_str!("../../tests/fixtures/screen_styled_lead-idle-shells_2.1.325.txt");
const WORKING: &str =
    include_str!("../../tests/fixtures/screen_styled_lead-working-shells_2.1.325.txt");
const NOTICE: &str = "✘ Auto-update failed · Run claude doctor";

fn control(text: &str) -> ControlScreen {
    let text = if text.starts_with("Captured:") {
        text.split_once('\n').unwrap().1
    } else {
        text
    };
    observe_control_screen(ScreenInput::Styled(text), None)
}

fn footer(text: &str, value: &str) -> String {
    let mut rows: Vec<_> = text.lines().collect();
    rows.pop();
    format!("{}\n  {value}\n", rows.join("\n"))
}

fn notice(text: &str, replacement: &str) -> String {
    text.lines()
        .map(|line| {
            if line.contains("Auto-update failed") {
                replacement
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn assert_editor(text: &str, turn: TurnState, input: InputState) {
    let observed = control(text);
    assert_eq!(observed.dialog, ControlDialog::None, "{text}");
    assert_eq!(observed.facts.turn, turn);
    assert_eq!(observed.facts.input, input);
    assert_eq!(observed.facts.dialog, DialogState::None);
    assert_eq!(observed.facts.fresh_editor, FreshEditorEvidence::Unknown);
}

#[test]
fn lead_captured_working_and_idle_chrome_are_recognized() {
    // Removing either chrome recognizer breaks these unchanged captured inputs.
    assert_editor(IDLE, TurnState::NotRunning, InputState::Empty);
    assert_editor(WORKING, TurnState::Running, InputState::Empty);
}

#[test]
fn lead_shell_footer_and_update_notice_are_independent() {
    assert_editor(&notice(IDLE, ""), TurnState::NotRunning, InputState::Empty);
    assert_editor(
        &footer(IDLE, "⏵⏵ auto mode on (shift+tab to cycle)"),
        TurnState::NotRunning,
        InputState::Empty,
    );
}

#[test]
fn lead_shell_counts_modes_and_agents_hint_follow_the_verified_bundle() {
    // Synthetic: value-only forms verified in the installed 2.1.325 bundle.
    // sds: n === 1 ? "1 shell" : `${n} shells`; PM/normal footer assembly
    // puts the agents hint before or after the task pill, in auto or manual mode.
    for mode in ["⏵⏵ auto mode on", "⏸ manual mode on"] {
        for count in [
            "1 shell",
            "2 shells",
            "15 shells",
            "12345678901234567890 shells",
        ] {
            for suffix in [
                count.to_owned(),
                format!("{count} · ← for agents"),
                format!("← for agents · {count}"),
            ] {
                assert_editor(
                    &footer(IDLE, &format!("{mode} · {suffix}")),
                    TurnState::NotRunning,
                    InputState::Empty,
                );
            }
        }
    }
}

#[test]
fn lead_unknown_or_malformed_footer_forms_stay_unknown() {
    // Synthetic: a starts_with match, numeric parse fallback or dropped words
    // must not turn a novel footer into permission to type.
    for suffix in [
        "0 shells",
        "0 shell",
        "1 shells",
        "2 shell",
        "01 shells",
        "-2 shells",
        "+2 shells",
        "2.0 shells",
        "２ shells",
        "2  shells",
        "2 shells novel",
        "2 monitors",
        "2 shells, 1 monitor",
        "2 background tasks",
        "2 shells · ↓ to view",
        "2 shells · ← for agent",
        "← for agents · 2 shells · ← for agents",
        "2 shells · novel",
        "2 shells · 3 shells",
    ] {
        assert_eq!(
            control(&footer(IDLE, &format!("⏵⏵ auto mode on · {suffix}"))).dialog,
            ControlDialog::Unknown,
            "{suffix}"
        );
    }
    for value in [
        "⏵⏵ bypass permissions on · 2 shells",
        "⏵⏵ auto mode off · 2 shells",
        "⏵⏵ auto mode on (shift+tab to cycle) · 2 shells",
        "⏸ manual mode on · 2 shell",
    ] {
        assert_eq!(
            control(&footer(IDLE, value)).dialog,
            ControlDialog::Unknown,
            "{value}"
        );
    }
    let selector = format!("{IDLE}\n  ● main\n  ◯ general-purpose\n");
    assert_eq!(control(&selector).dialog, ControlDialog::Unknown);
}

#[test]
fn lead_update_notice_requires_its_exact_aligned_pre_border_slot() {
    // Synthetic: broad trimming or skipping arbitrary status/history rows
    // would make at least one damaged frame below look safe.
    let row = format!("{:>142}", NOTICE);
    for invalid in [
        format!(" {row}"),
        row.trim_start().to_owned(),
        format!("{row} novel"),
        row.replace("claude doctor", "claude update"),
        row.replace("failed", "ready"),
        row.replace("✘", "!"),
        format!("{row}\n"),
        format!("{row}\n{row}"),
        format!("{row}\nUnknown progress"),
    ] {
        assert_eq!(
            control(&notice(IDLE, &invalid)).dialog,
            ControlDialog::Unknown,
            "{invalid}"
        );
    }
    let displaced = notice(IDLE, "").replace("❯\u{a0}", &format!("{row}\n❯\u{a0}"));
    assert_eq!(control(&displaced).dialog, ControlDialog::Unknown);
}

#[test]
fn lead_chrome_tracks_rule_geometry_not_absolute_rows_or_status_values() {
    // Synthetic: complete chrome moved/reflowed, not a claim of real resize fidelity.
    for width in [80, 120, 144, 200] {
        for (source, turn) in [(IDLE, TurnState::NotRunning), (WORKING, TurnState::Running)] {
            let original = StyledScreen::parse(source).unwrap();
            let resized = original
                .text()
                .lines()
                .map(|line| {
                    if line.starts_with('─') {
                        "─".repeat(width)
                    } else if line.contains("Auto-update failed") {
                        format!("{:>field$}", NOTICE, field = width - 2)
                    } else if line.contains("[fixture] ctx") {
                        "  arbitrary status values /clear  not a popup".to_owned()
                    } else {
                        line.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert_editor(
                &format!("old transcript\n\n{resized}\n"),
                turn,
                InputState::Empty,
            );
        }
    }
}

#[test]
fn lead_drafts_and_unknown_active_progress_are_not_made_idle() {
    let draft = IDLE.replace("❯\u{a0}", "❯\u{a0}typed draft");
    assert_editor(&draft, TurnState::NotRunning, InputState::Draft);
    let working_draft = WORKING.replace("❯\u{a0}", "❯\u{a0}typed draft");
    assert_editor(&working_draft, TurnState::Running, InputState::Draft);
    let unknown = IDLE
        .lines()
        .map(|line| {
            if line.contains("Worked for") {
                "Unknown active progress"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_ne!(unknown, IDLE.trim_end());
    assert_eq!(control(&unknown).dialog, ControlDialog::Unknown);
}
