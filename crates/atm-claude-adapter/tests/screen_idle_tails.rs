//! Captured: Claude Code 2.1.325 and 2.1.328, 2026-11-19, sealed local canned
//! Messages API, tmux 3.8 (`capture-pane -p -e -N`). Cropped to rows 15–40;
//! only the opaque command cells in compaction hook rows were redacted.

use atm_claude_adapter::screen;
use atm_core::harness_control::{ControlDialog, ControlScreen};
use atm_core::styled_screen::StyledScreen;
use atm_core::{DialogState, InputState, ScreenFacts, ScreenInput, TurnState};

const COMPACT_325: &str = include_str!("fixtures/claude_compact_done_2.1.325.styled.txt");
const COMPACT_328: &str = include_str!("fixtures/claude_compact_done_2.1.328.styled.txt");
const WARNING_325: &str = include_str!("fixtures/claude_stop_warning_done_2.1.325.styled.txt");
const WARNING_328: &str = include_str!("fixtures/claude_stop_warning_done_2.1.328.styled.txt");

fn body(capture: &str) -> &str {
    let Some((provenance, text)) = capture.split_once('\n') else {
        panic!("expected capture with provenance");
    };
    assert!(provenance.starts_with("Captured: Claude Code "));
    text
}

fn visible(capture: &str) -> String {
    let Ok(screen) = StyledScreen::parse(body(capture)) else {
        panic!("expected valid captured style");
    };
    screen
        .text()
        .lines()
        .map(|line| line.trim_end_matches(' '))
        .collect::<Vec<_>>()
        .join("\n")
}

#[track_caller]
fn assert_editor(text: &str, turn: TurnState, case: &str) {
    let input = ScreenInput::Styled(text);
    let mut expected = ScreenFacts::default();
    expected.input = InputState::Empty;
    expected.dialog = DialogState::None;
    expected.turn = turn;
    let facts = screen::observe_screen(input, None);
    let control = screen::observe_control_screen(input, None);
    assert_eq!(
        (facts, control.facts),
        (expected.clone(), expected),
        "{case}"
    );
    assert_eq!(
        control.dialog,
        if turn == TurnState::Unknown {
            ControlDialog::Unknown // These fixtures omit the startup header.
        } else {
            ControlDialog::None
        },
        "{case}"
    );
    assert_eq!(
        screen::staged_draft_matches(input, "/clear"),
        None,
        "{case}"
    );
}

#[test]
fn captured_completed_compaction_325() {
    assert_editor(body(COMPACT_325), TurnState::NotRunning, "2.1.325 compact");
}

#[test]
fn captured_completed_compaction_328() {
    assert_editor(body(COMPACT_328), TurnState::NotRunning, "2.1.328 compact");
}

#[test]
fn captured_settled_stop_warning_325() {
    assert_editor(body(WARNING_325), TurnState::NotRunning, "2.1.325 warning");
}

#[test]
fn captured_settled_stop_warning_328() {
    assert_editor(body(WARNING_328), TurnState::NotRunning, "2.1.328 warning");
}

#[test]
fn captured_in_progress_compaction_stays_unknown() {
    for (capture, case) in [
        (
            include_str!("fixtures/claude_compact_busy_2.1.325.styled.txt"),
            "2.1.325 compacting",
        ),
        (
            include_str!("fixtures/claude_compact_busy_2.1.328.styled.txt"),
            "2.1.328 compacting",
        ),
    ] {
        assert_editor(body(capture), TurnState::Unknown, case);
    }
}

#[test]
fn captured_warning_while_stop_hook_runs_stays_unknown() {
    for (capture, case) in [
        (
            include_str!("fixtures/claude_stop_hook_busy_2.1.325.styled.txt"),
            "2.1.325 Stop hook",
        ),
        (
            include_str!("fixtures/claude_stop_hook_busy_2.1.328.styled.txt"),
            "2.1.328 Stop hook",
        ),
    ] {
        assert_editor(body(capture), TurnState::Unknown, case);
    }
}

#[test]
fn captured_stop_failure_stays_unknown() {
    let capture = include_str!("fixtures/claude_stop_failure_2.1.328.styled.txt");
    assert_editor(body(capture), TurnState::Unknown, "2.1.328 API error");
}

#[test]
fn manual_compaction_does_not_need_the_hook_output() {
    // Synthetic: remove only the four hook output rows from each completed
    // capture. The submitted command, exact marker and prior done stay visible.
    for capture in [COMPACT_325, COMPACT_328] {
        let text = visible(capture);
        let without_hooks = text
            .lines()
            .filter(|row| !row.starts_with("     ") || row.starts_with("      "))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.lines().count(), without_hooks.lines().count() + 4);
        assert_editor(&without_hooks, TurnState::NotRunning, "no hook rows");
    }
}

#[track_caller]
fn change(text: &str, old: &str, new: &str) -> String {
    assert!(text.contains(old), "the test must actually change {old:?}");
    text.replacen(old, new, 1)
}

#[test]
fn stop_warning_must_be_exact_aligned_and_over_done() {
    // Synthetic: a lookalike or displaced warning is not chrome; even the
    // exact warning cannot expose stale completion or substitute for done.
    let text = visible(WARNING_328);
    const WARNING: &str = "stop hook reported a problem · ctrl+o to see";
    const DONE: &str = "✻ Crunched for 1s · done 7:39 AM";
    for (old, new, case) in [
        (
            WARNING,
            "stop hook reported a warning · ctrl+o to see",
            "new warning wording",
        ),
        (&format!(" {WARNING}"), WARNING, "new right margin"),
        (
            &format!("{WARNING}\n"),
            &format!("{WARNING}\n\n"),
            "warning not next to rule",
        ),
        (DONE, "", "no completed spinner"),
        (
            DONE,
            &format!("{DONE}\n● Fixture reply 3."),
            "newer response",
        ),
        (
            DONE,
            "  ✻ Crunched for 1s · done 7:39 AM",
            "indented spinner lookalike",
        ),
    ] {
        assert_editor(&change(&text, old, new), TurnState::Unknown, case);
    }
}

#[test]
fn compacted_marker_must_follow_manual_command_and_done() {
    // Synthetic: a marker is not completion evidence on its own, nor is a
    // previous done inherited across unrecognized progress or a newer prompt.
    let text = visible(COMPACT_328);
    const MARKER: &str = "  ⎿ \u{a0}Compacted (ctrl+o to see full summary)";
    const DONE: &str = "✻ Worked for 0s · done 7:39 AM";
    for (old, new, case) in [
        (
            MARKER,
            "  ⎿ \u{a0}Compacted (ctrl+o for details)",
            "new marker wording",
        ),
        (
            MARKER,
            "   ⎿ \u{a0}Compacted (ctrl+o to see full summary)",
            "marker shifted",
        ),
        ("❯ /compact\n", "", "no manual command"),
        (
            "❯ /compact",
            "❯ /compact notes",
            "unverified command variant",
        ),
        ("❯ /compact\n", "❯ /compact\n\n", "disconnected marker"),
        (DONE, "", "no completed spinner"),
        (
            DONE,
            &format!("{DONE}\n❯ synthetic timing turn"),
            "newer submitted prompt",
        ),
        (
            DONE,
            &format!("{DONE}\n✻ Compacting conversation… (0s)"),
            "intervening progress",
        ),
    ] {
        assert_editor(&change(&text, old, new), TurnState::Unknown, case);
    }
}

#[test]
fn compact_hook_output_must_be_one_exact_success_pair() {
    // Synthetic: neither unknown rows in the block nor partial, misaligned,
    // reordered or failed hook output can be skipped to reach an earlier done.
    let text = visible(COMPACT_328);
    let reversed = text
        .replace("PreCompact [", "FirstCompact [")
        .replace("PostCompact [", "PreCompact [")
        .replace("FirstCompact [", "PostCompact [");
    assert_editor(&reversed, TurnState::Unknown, "swapped hooks");
    for (old, new, case) in [
        ("     PreCompact [", "    PreCompact [", "misaligned hook"),
        (
            "     PostCompact [",
            "     Novel notice\n     PostCompact [",
            "extra row",
        ),
        (
            "     PostCompact [",
            "     HookProgress [",
            "unknown second hook",
        ),
        (
            "] completed successfully: {}",
            "] reported a problem: {}",
            "hook error",
        ),
    ] {
        assert_editor(&change(&text, old, new), TurnState::Unknown, case);
    }
    let post = text
        .lines()
        .position(|row| row.starts_with("     PostCompact ["))
        .expect("PostCompact record");
    let partial = text
        .lines()
        .enumerate()
        .filter_map(|(i, row)| (!(post..post + 2).contains(&i)).then_some(row))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!partial.contains("PostCompact [") && partial.contains("PreCompact ["));
    assert_editor(&partial, TurnState::Unknown, "one hook only");
}

#[test]
fn completed_tails_do_not_override_busy_footer() {
    // Synthetic: keep the captured completed transcript and add only an
    // otherwise recognized busy footer; the contradiction must refuse it.
    for capture in [COMPACT_328, WARNING_328] {
        let busy = change(
            &visible(capture),
            "(shift+tab to cycle) · ← for agents",
            "(shift+tab to cycle) · esc to interrupt · ← for agents",
        );
        let input = ScreenInput::Styled(&busy);
        assert_eq!(screen::observe_screen(input, None), ScreenFacts::default());
        assert_eq!(
            screen::observe_control_screen(input, None),
            ControlScreen::default()
        );
    }
}
