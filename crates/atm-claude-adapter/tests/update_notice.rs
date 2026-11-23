//! Captured: Claude Code 2.1.325, 2026-11-17, read-only tmux capture-pane -p -e -N
//! of an idle seat (supplied by the lead), redacted. Bodies retain 184x11 geometry.
use atm_claude_adapter::screen::{observe_control_screen, observe_screen, staged_draft_matches};
use atm_core::harness_control::ControlDialog;
use atm_core::styled_screen::StyledScreen;
use atm_core::{DialogState, FreshEditorEvidence, InputState, ScreenFacts, ScreenInput, TurnState};

const COMBINED: &str = include_str!("fixtures/screen_styled_clear-update-installed_2.1.325.txt");
const STANDALONE: &str = include_str!("fixtures/screen_styled_update-installed_2.1.325.txt");
const STANDALONE_IDLE_330: &str =
    include_str!("fixtures/screen_styled_update-installed-idle_2.1.330.txt");
const STANDALONE_RUNNING_330: &str =
    include_str!("fixtures/screen_styled_update-installed-running_2.1.330.txt");
const UPDATE: &str = "✔ Update installed · /restart to apply";
const RESTART_TO_UPDATE: &str = "✔ Update installed · Restart to update";

fn restart_to_update_capture() -> String {
    capture_grid(
        include_str!("fixtures/screen_styled_restart-to-update_2.1.325.txt"),
        7,
    )
}

fn capture_grid(capture: &str, rows: usize) -> String {
    let mut frame = String::new();
    for row in body(capture).lines() {
        let Some(row) = row.strip_suffix('|') else {
            panic!("fixture row sentinel");
        };
        frame.push_str(row);
        frame.push('\n');
    }
    assert_eq!(frame.lines().count(), rows);
    frame
}

fn body(capture: &str) -> &str {
    capture.split_once('\n').map_or(capture, |(_, body)| body)
}
fn footer(capture: &str) -> String {
    // Synthetic: the supplied short viewports have no mode footer. Append the
    // already-captured auto-mode footer to isolate notice recognition, not relax it.
    format!("{}⏵⏵ auto mode on (shift+tab to cycle)\n", body(capture))
}
fn plain(text: &str) -> String {
    StyledScreen::parse(text)
        .unwrap_or_else(|error| panic!("invalid fixture styles: {error}"))
        .text()
        .to_owned()
}
fn notice(text: &str, replacement: &str) -> String {
    // Synthetic: mutate only the notice, retaining its captured two-cell margin.
    text.lines()
        .map(|line| {
            if line.contains("✔ Update installed") {
                format!(
                    "{}{}  ",
                    " ".repeat(182 - replacement.chars().count()),
                    replacement
                )
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn facts(text: &str) -> ScreenFacts {
    observe_control_screen(ScreenInput::Styled(text), None).facts
}
fn idle(text: &str) {
    let control = observe_control_screen(ScreenInput::Styled(text), None);
    assert_eq!(control.dialog, ControlDialog::None);
    assert_eq!(control.facts.input, InputState::Empty);
    assert_eq!(control.facts.turn, TurnState::NotRunning);
    assert_eq!(control.facts.dialog, DialogState::None);
    assert_eq!(control.facts.fresh_editor, FreshEditorEvidence::Unknown);
    assert_eq!(
        observe_screen(ScreenInput::Styled(text), None),
        control.facts
    );
    assert_eq!(
        staged_draft_matches(ScreenInput::Styled(text), "/clear"),
        None
    );
}

#[test]
fn combined_update_notice_preserves_completed_turn_with_known_footer() {
    idle(&footer(COMBINED));
}

#[test]
fn standalone_update_notice_preserves_completed_turn_with_known_footer() {
    idle(&footer(STANDALONE));
}

// Captured: Claude Code 2.1.330, 2026-11-22, read-only tmux capture-pane -p -e -N.
// A lone update notice must not hide the completed turn in the real idle frame.
#[test]
fn captured_330_standalone_update_notice_preserves_idle_turn() {
    let text = capture_grid(STANDALONE_IDLE_330, 40);
    assert!(text.contains(UPDATE));
    idle(&text);
}

// Captured: Claude Code 2.1.330, 2026-11-22, read-only tmux capture-pane -e.
// The same notice and empty editor must not turn a running seat into an idle one.
#[test]
fn captured_330_standalone_update_notice_preserves_running_turn() {
    let text = capture_grid(STANDALONE_RUNNING_330, 8);
    assert!(text.contains(UPDATE));
    let input = ScreenInput::Styled(&text);
    let control = observe_control_screen(input, None);
    assert_eq!(control.dialog, ControlDialog::None);
    assert_eq!(control.facts.input, InputState::Empty);
    assert_eq!(control.facts.turn, TurnState::Running);
    assert_eq!(control.facts.dialog, DialogState::None);
    assert_eq!(observe_screen(input, None), control.facts);
}

#[test]
fn captured_restart_to_update_notice_preserves_completed_turn() {
    let text = restart_to_update_capture();
    assert!(text.contains(RESTART_TO_UPDATE));
    idle(&text);
}

#[test]
fn restart_to_update_notice_does_not_make_a_typed_draft_empty() {
    let text = restart_to_update_capture();
    assert_eq!(text.matches("\x1b[2m").count(), 1);
    // Synthetic: remove only faint from the editor glyphs to model a typed draft.
    let text = text.replace("\x1b[2m", "");
    let input = ScreenInput::Styled(&text);
    let control = observe_control_screen(input, None);
    assert_eq!(control.facts.input, InputState::Draft);
    assert_eq!(control.facts.turn, TurnState::NotRunning);
    assert_eq!(control.dialog, ControlDialog::None);
    assert_eq!(control.facts.fresh_editor, FreshEditorEvidence::Unknown);
    assert_eq!(
        staged_draft_matches(input, "I, can see if our page loads"),
        Some(true)
    );
    assert_eq!(staged_draft_matches(input, "/clear"), Some(false));
}

#[test]
fn restart_to_update_notice_rejects_wrong_spelling_and_alignment() {
    let text = restart_to_update_capture();
    let row = text
        .lines()
        .find(|row| row.contains(RESTART_TO_UPDATE))
        .unwrap();
    // Synthetic: two equal-width wrong spellings, a one-cell left shift, and a
    // row inserted before the border. No variant changes the footer or editor.
    for changed in [
        text.replace(RESTART_TO_UPDATE, "✔ Update installed · Restart to udpate"),
        text.replace(RESTART_TO_UPDATE, "✔ Update installed · Restart to resume"),
        text.replace(row, row.strip_prefix(' ').unwrap()),
        text.replace(row, &format!("{row}\n")),
    ] {
        assert_ne!(changed, text);
        let control = observe_control_screen(ScreenInput::Styled(&changed), None);
        assert_eq!(control.facts.turn, TurnState::Unknown);
        assert_eq!(control.dialog, ControlDialog::Unknown);
    }
}

#[test]
fn captured_footerless_viewports_stay_unknown() {
    for capture in [COMBINED, STANDALONE] {
        let input = ScreenInput::Styled(body(capture));
        assert_eq!(observe_control_screen(input, None), Default::default());
        assert_eq!(observe_screen(input, None), Default::default());
        assert_eq!(staged_draft_matches(input, "/clear"), None);
    }
}

#[test]
fn notice_never_supplies_turn_input_or_freshness_evidence() {
    for capture in [COMBINED, STANDALONE] {
        let text = plain(&footer(capture));
        // Synthetic: change only the captured completed-turn status or editor.
        let changed_status = |status: &str| {
            text.lines()
                .map(|line| {
                    if line.contains(" · done ") {
                        status
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let working = changed_status("✻ Working…");
        assert_eq!(facts(&working).turn, TurnState::Running);
        assert_eq!(facts(&working).fresh_editor, FreshEditorEvidence::Unknown);
        for tail in ["unrecognized progress", ""] {
            let unknown = changed_status(tail);
            assert_eq!(facts(&unknown).turn, TurnState::Unknown);
            assert_eq!(
                observe_control_screen(ScreenInput::Styled(&unknown), None).dialog,
                ControlDialog::Unknown
            );
        }
        let draft = text.replace("❯\u{a0}", "❯\u{a0}typed draft");
        assert_ne!(draft, text);
        assert_eq!(facts(&draft).input, InputState::Draft);
        assert_eq!(facts(&draft).turn, TurnState::NotRunning);
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&draft), "/clear"),
            Some(false)
        );
    }
}

#[test]
fn partial_other_or_misaligned_notices_stay_unknown() {
    for capture in [COMBINED, STANDALONE] {
        let text = footer(capture);
        // Synthetic: valid terminal chrome must not admit partial/novel notice text.
        for invalid in [
            "✔ Update installed",
            "✔ Update installed · /restart",
            "✔ Update installed · /restart to apply extra",
            "✔ Update available · /restart to apply",
            "prefix ✔ Update installed · /restart to apply",
            "new task? /clear to save 187k tokens ✔ Update installed · /restart to apply",
            "new task? /clear to save 187k tokens · ✔ Update installed",
            "new task? /clear to save 187k tokens · other notice",
        ] {
            let changed = notice(&text, invalid);
            assert_eq!(facts(&changed).turn, TurnState::Unknown, "{invalid}");
            assert_eq!(
                observe_control_screen(ScreenInput::Styled(&changed), None).dialog,
                ControlDialog::Unknown,
                "{invalid}"
            );
        }
        let row = text
            .lines()
            .find(|line| line.contains("✔ Update installed"))
            .unwrap();
        // Synthetic: either margin moves, or a blank/progress row separates notice and border.
        for changed in [
            text.replace(row, &format!(" {row}")),
            text.replace(row, row.strip_prefix(' ').unwrap()),
            text.replace(row, &format!("{row}\n")),
            text.replace(row, &format!("{row}\nunrecognized progress")),
        ] {
            assert_eq!(facts(&changed).turn, TurnState::Unknown);
            assert_eq!(
                observe_control_screen(ScreenInput::Styled(&changed), None).dialog,
                ControlDialog::Unknown
            );
        }
    }
}

#[test]
fn combined_notice_reuses_existing_count_shapes() {
    let text = footer(COMBINED);
    // Synthetic: scalar variations use the existing formatter grammar, not a fixed 187k.
    for count in ["1", "1k", "1.2m", "187k", "2b", "9.8t"] {
        idle(&notice(
            &text,
            &format!("new task? /clear to save {count} tokens · {UPDATE}"),
        ));
    }
    for count in ["0", "01k", "1.0k", "1.22k", "1K", "-1", "1e3", "unknown"] {
        let changed = notice(
            &text,
            &format!("new task? /clear to save {count} tokens · {UPDATE}"),
        );
        assert_eq!(facts(&changed).turn, TurnState::Unknown, "{count}");
    }
}
