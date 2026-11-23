//! The idle `/clear` hint beside an update notice on Claude Code 2.1.327
//! (unit s3-busy). Each fixture's first line is its `Captured:` provenance:
//! a sealed rig (private user and network namespace, canned Messages API),
//! the auto-updater on with no network, tmux 3.8 `capture-pane -p -e`. Right
//! after a turn the failure notice stands alone above the editor; once the
//! seat is idle Claude joins the hint to it on one right-aligned row. Read as
//! unknown, that row held every delivery to an idle seat until its next turn.
use atm_claude_adapter::screen::{observe_control_screen, observe_screen};
use atm_core::harness_control::ControlDialog;
use atm_core::{DialogState, InputState, ScreenInput, TurnState};

const AFTER_TURN: &str = include_str!("fixtures/claude_update_failed_82x60_2.1.327.styled.txt");
const IDLE_HINT: &str = include_str!("fixtures/claude_update_failed_hint_82x60_2.1.327.styled.txt");
const FAILED: &str = "✘ Auto-update failed · Run claude doctor";
const COLUMNS: usize = 82;

/// A row's text without its SGR styling: the notice words carry their own colours.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn body(capture: &str) -> &str {
    match capture.split_once('\n') {
        Some((first, rest)) if first.starts_with("Captured:") => rest,
        _ => panic!("expected a provenance line above the capture"),
    }
}

/// The idle frame with its hint row replaced by `text`, right-aligned with
/// `margin` cells after it (Claude's own margin is two).
fn with_hint_row(text: &str, margin: usize) -> String {
    let frame = body(IDLE_HINT);
    let mut rows: Vec<String> = frame.lines().map(str::to_owned).collect();
    let index = rows
        .iter()
        .position(|row| plain(row).contains("new task? /clear to save"))
        .unwrap_or_else(|| panic!("the captured hint row"));
    let pad = COLUMNS - margin - text.chars().count();
    rows[index] = format!("{}{text}", " ".repeat(pad));
    rows.join("\n") + "\n"
}

fn read(text: &str) -> (ControlDialog, InputState, TurnState, DialogState) {
    let input = ScreenInput::Styled(text);
    let control = observe_control_screen(input, None);
    assert_eq!(observe_screen(input, None), control.facts);
    (
        control.dialog,
        control.facts.input,
        control.facts.turn,
        control.facts.dialog,
    )
}

const IDLE: (ControlDialog, InputState, TurnState, DialogState) = (
    ControlDialog::None,
    InputState::Empty,
    TurnState::NotRunning,
    DialogState::None,
);

// Break: the failure notice standing alone after a turn stops being chrome.
#[test]
fn update_failure_alone_after_a_turn_is_an_idle_editor() {
    let text = body(AFTER_TURN);
    assert!(plain(text).contains(FAILED) && !plain(text).contains("new task?"));
    assert_eq!(read(text), IDLE);
}

// s3-busy. Break: the idle hint joined to the failure notice reads as an
// unknown turn, so an idle seat takes no message until its next turn.
#[test]
fn idle_hint_beside_the_update_failure_is_an_idle_editor() {
    let text = body(IDLE_HINT);
    assert!(plain(text).contains(&format!("new task? /clear to save 11 tokens · {FAILED}")));
    assert_eq!(read(text), IDLE);
}

// s3-busy (the live chief's row). Synthetic: built from the real frame above
// with the notice the lead read on the live 2.1.327 chief seat, 2026-11-21
// ("✔ Update installed · Restart to update"), in Claude's two-cell margin.
#[test]
fn idle_hint_beside_restart_to_update_is_an_idle_editor() {
    let row = "new task? /clear to save 11 tokens · ✔ Update installed · Restart to update";
    assert_eq!(read(&with_hint_row(row, 2)), IDLE);
}

// Synthetic, from the real frame: a hint cut to fit beside the failure notice
// at 82 columns still reads as the hint, as it does beside "/restart to apply".
#[test]
fn cut_idle_hint_beside_the_update_failure_is_an_idle_editor() {
    let row = format!("new task? /clear to save 410.5k t… · {FAILED}");
    assert_eq!(read(&with_hint_row(&row, 2)), IDLE);
}

// Break: any notice beside the hint, or the right notice out of Claude's
// alignment, becomes chrome. Unknown rows must keep holding delivery.
#[test]
fn unknown_notice_or_alignment_beside_the_hint_stays_unknown() {
    for (case, row, margin) in [
        (
            "unknown notice",
            "new task? /clear to save 11 tokens · ✘ Something else · Run claude doctor".to_owned(),
            2,
        ),
        (
            "misaligned",
            format!("new task? /clear to save 11 tokens · {FAILED}"),
            3,
        ),
    ] {
        let (_, _, turn, _) = read(&with_hint_row(&row, margin));
        assert_eq!(turn, TurnState::Unknown, "{case}");
    }
}
