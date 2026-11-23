//! Prompt suggestions and the idle `/clear` hint on Claude Code 2.1.327, 2.1.329
//! and 2.1.330 (unit s3-stuckseat). Each fixture's first line is its `Captured:`
//! provenance: a sealed rig (private user and network namespace, canned Messages
//! API, `CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION=1`, the idle hint thresholds
//! lowered by `CLAUDE_CODE_IDLE_THRESHOLD_MINUTES` and
//! `CLAUDE_CODE_IDLE_TOKEN_THRESHOLD`), tmux 3.8 `capture-pane -p -e -N`.
//!
//! A suggestion in an unfocused pane is wholly faint, with no inverse cursor
//! glyph: Claude draws that glyph only while its pane has terminal focus (seen
//! live on a seat stuck since a restart, and reproduced here with focus-events
//! on and another window active).
use atm_claude_adapter::screen::{observe_control_screen, observe_screen, staged_draft_matches};
use atm_core::harness_control::ControlDialog;
use atm_core::{DialogState, InputState, ScreenFacts, ScreenInput, TurnState};

const UNFOCUSED_327: &str =
    include_str!("fixtures/claude_suggestion_unfocused_82x60_2.1.327.styled.txt");
const UNFOCUSED_329: &str =
    include_str!("fixtures/claude_suggestion_unfocused_82x60_2.1.329.styled.txt");
const FOCUSED_330: &str =
    include_str!("fixtures/claude_suggestion_focused_82x60_2.1.330.styled.txt");
const DRAFT_329: &str = include_str!("fixtures/claude_typed_draft_200x50_2.1.329.styled.txt");

fn body(capture: &str) -> &str {
    match capture.split_once('\n') {
        Some((first, rest)) if first.starts_with("Captured:") => rest,
        _ => panic!("expected a provenance line above the capture"),
    }
}

/// A row's text without its SGR styling: the hint's words carry their own colours.
fn plain(row: &str) -> String {
    let mut out = String::new();
    let mut chars = row.chars();
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

fn hint_row(text: &str) -> Option<&str> {
    text.lines()
        .find(|line| plain(line).contains("new task? /clear to save "))
}

fn idle_editor(text: &str) {
    let input = ScreenInput::Styled(text);
    let control = observe_control_screen(input, None);
    assert_eq!(control.dialog, ControlDialog::None);
    assert_eq!(control.facts.input, InputState::Empty);
    assert_eq!(control.facts.turn, TurnState::NotRunning);
    assert_eq!(control.facts.dialog, DialogState::None);
    assert_eq!(observe_screen(input, None), control.facts);
    // A suggestion is never a staged draft, whatever its text.
    assert_eq!(
        staged_draft_matches(input, "go ahead with tasks 10-13"),
        None
    );
}

// Break: the unfocused, wholly faint suggestion read as a typed draft, so a
// seat that shows it is never a safe editor.
#[test]
fn unfocused_suggestion_with_the_idle_hint_is_an_empty_idle_editor() {
    for capture in [UNFOCUSED_327, UNFOCUSED_329] {
        let text = body(capture);
        assert!(text.contains("\x1b[2mgo ahead with tasks 10-13"));
        assert!(
            !text.contains("\x1b[7m"),
            "no inverse cursor when unfocused"
        );
        assert!(hint_row(text).is_some(), "the idle hint is shown");
        idle_editor(text);
    }
}

#[test]
fn focused_suggestion_with_the_idle_hint_is_an_empty_idle_editor() {
    let text = body(FOCUSED_330);
    assert!(text.contains("\x1b[7mg\x1b[0;2mo ahead with tasks 10-13"));
    idle_editor(text);
}

// Break: removing the faint style from a suggestion still reads Empty (a typed
// line is never SGR 2), or the captured one-letter draft reads Empty.
#[test]
fn typed_text_stays_a_draft() {
    for capture in [UNFOCUSED_327, UNFOCUSED_329] {
        // Synthetic: the same row without SGR 2 is typed text.
        let typed = body(capture).replace("\x1b[2mgo ahead", "go ahead");
        assert_eq!(
            observe_control_screen(ScreenInput::Styled(&typed), None)
                .facts
                .input,
            InputState::Draft
        );
    }
    let facts: ScreenFacts = observe_screen(ScreenInput::Styled(body(DRAFT_329)), None);
    assert_eq!(facts.input, InputState::Draft);
}

fn with_hint_row(capture: &str, notice: &str) -> String {
    // Synthetic: replace only the captured hint row, keeping its two-cell
    // right margin at 82 columns.
    let text = body(capture);
    let Some(row) = hint_row(text) else {
        panic!("the capture shows the idle hint");
    };
    let changed = text.replace(
        row,
        &format!("{}{notice}  ", " ".repeat(80 - notice.chars().count())),
    );
    assert_ne!(changed, text);
    changed
}

// Synthetic: a live 82-column seat (Claude Code, version not recorded,
// 2026-11-20, read-only capture by the lead, not committed: it holds another
// project's text) showed the hint cut to fit beside the update notice. The
// sealed rig cannot install an update, so the row is rebuilt here on the
// rig's own frame. Break: the cut hint is not chrome, so the seat's turn is
// Unknown and it never becomes a safe editor.
const UPDATE: &str = "✔ Update installed · /restart to apply";

/// The hint cut beside the update notice, as at 82 columns: Claude cuts it to
/// fill exactly 78 cells between its two-cell margins.
fn cut_beside_update(kept: &str) -> String {
    let row = format!("new task? /clear to save {kept}… · {UPDATE}");
    assert_eq!(row.chars().count(), 78, "{row}");
    row
}

#[test]
fn hint_cut_to_fit_beside_the_update_notice_is_chrome() {
    // The live row, character for character.
    let live = "new task? /clear to save 480.5k toke… · ✔ Update installed · /restart to apply";
    assert_eq!(cut_beside_update("480.5k toke"), live);
    for capture in [UNFOCUSED_327, UNFOCUSED_329, FOCUSED_330] {
        idle_editor(&with_hint_row(capture, live));
        // Synthetic: the same cut after other whole counts.
        for kept in ["480.5 token", "9876543210k", "98765432 to", "1234567 tok"] {
            idle_editor(&with_hint_row(capture, &cut_beside_update(kept)));
        }
        // Synthetic: a short hint cut alone, at the captured right margin.
        idle_editor(&with_hint_row(capture, "new task? /clear to save 12 toke…"));
    }
}

// Break: any text ending in an ellipsis passes as the hint, or a cut count.
#[test]
fn only_a_cut_tail_of_a_whole_count_is_accepted() {
    for capture in [UNFOCUSED_327, FOCUSED_330] {
        // Synthetic: aligned exactly as the live row, so only the text differs.
        let mut invalid: Vec<String> = [
            "480.5k tokx",
            "0480.5 toke",
            "480.55 toke",
            "480.5K toke",
            "480.5k ttok",
            "4805.5. tok",
        ]
        .iter()
        .map(|kept| cut_beside_update(kept))
        .collect();
        // Synthetic: aligned exactly, but cut with no ellipsis.
        invalid.push(format!("new task? /clear to save 480.5k token · {UPDATE}"));
        invalid.push("new task? /clear to save 0 toke…".into());
        invalid.push("new task? /clear to save toke…".into());
        for row in &invalid {
            let changed = with_hint_row(capture, row);
            let control = observe_control_screen(ScreenInput::Styled(&changed), None);
            // An unknown turn is never a safe editor.
            assert_eq!(control.facts.turn, TurnState::Unknown, "{row}");
        }
    }
}

const RECAP_327: &str = include_str!("fixtures/claude_recap_unfocused_82x60_2.1.327.styled.txt");

fn recap_row(text: &str) -> &str {
    let Some(row) = text
        .lines()
        .find(|line| plain(line).starts_with("※ recap: "))
    else {
        panic!("the capture shows a recap");
    };
    row
}

// Break: the away recap Claude adds to an unfocused idle seat counts as
// transcript, so the seat's turn is Unknown and it never becomes safe.
#[test]
fn away_recap_above_the_idle_hint_is_chrome() {
    let text = body(RECAP_327);
    let row = plain(recap_row(text));
    assert_eq!(
        row.trim_end(),
        "※ recap: go ahead with tasks 10-13 (turn off recaps in /config)"
    );
    assert!(hint_row(text).is_some());
    idle_editor(text);
}

// Synthetic: the live seat's recap (same source as the cut hint above) wrapped
// over four rows, each continuation indented two cells; rebuilt here on the
// rig's frame with neutral text.
#[test]
fn wrapped_away_recap_is_chrome() {
    let text = body(RECAP_327);
    let wrapped = "※ recap: Getting the feature working: tasks 3 to 5 are live, and I am\n  holding for your decision. Next, run the measure while the model is\n  free, then tell me whether to build tasks 10 to 13, which add the\n  last step. (turn off recaps in /config)";
    idle_editor(&text.replace(recap_row(text), wrapped));
    let later = "※ recap: Tasks 3 to 5 are live; waiting for your decision on the rest.";
    idle_editor(&text.replace(recap_row(text), later));
}

// Break: any rows after a recap-like line are skipped, so a later unknown row
// (or a spinner) is hidden behind it.
#[test]
fn only_a_recap_block_is_skipped() {
    let text = body(RECAP_327);
    let row = recap_row(text);
    for changed in [
        // Synthetic: an unindented row below the recap.
        format!("{row}\nunrecognized progress"),
        // Synthetic: a misspelled or indented label.
        row.replace("recap: ", "recaps: "),
        format!(" {row}"),
        // Synthetic: a running spinner after the recap.
        format!("{row}\n✻ Churning… (3s)"),
    ] {
        let control =
            observe_control_screen(ScreenInput::Styled(&text.replace(row, &changed)), None);
        assert_ne!(control.facts.turn, TurnState::NotRunning, "{changed:?}");
    }
}
