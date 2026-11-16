//! Real startup/short-turn boundaries and synthetic refusal regressions.
#![cfg(test)]
use atm_core::harness_control::{ControlDialog, ControlScreen};
use atm_core::{
    styled_screen::StyledScreen, DialogState, FreshEditorEvidence, InputState, ScreenInput,
    TurnState,
};
use atm_pi_adapter::screen::{observe_control_screen, staged_draft_matches};

macro_rules! capture {
    ($name:literal, $version:literal) => {
        include_str!(concat!("fixtures/clear_", $name, "_pi_", $version, ".txt"))
    };
}
const POST_TURN: &str = capture!("post_turn", "1.5.0");
const STARTUP: &str = capture!("e2_startup", "1.5.0");

fn observe(text: &str) -> ControlScreen {
    observe_control_screen(ScreenInput::Styled(text), None)
}

// The existing atmd safe_editor contract; no session/freshness inference here.
fn safe(control: &ControlScreen) -> bool {
    control.dialog == ControlDialog::None
        && control.facts.input == InputState::Empty
        && control.facts.turn == TurnState::NotRunning
        && control.facts.dialog == DialogState::None
}

fn editor(text: &str, input: InputState, turn: TurnState) -> ControlScreen {
    let result = observe(text);
    assert_eq!(
        (result.facts.input, result.facts.turn, &result.facts.dialog),
        (input, turn, &DialogState::None),
        "{text}"
    );
    assert_eq!(result.dialog, ControlDialog::None, "{text}");
    result
}

fn plain(text: &str) -> String {
    StyledScreen::parse(text).unwrap().text().to_owned()
}

fn bounds(text: &str) -> (usize, usize, usize) {
    let text = plain(text);
    let lines: Vec<_> = text.lines().map(|s| s.trim_end_matches(' ')).collect();
    let lower = lines.iter().rposition(|s| s.starts_with("──")).unwrap();
    let upper = lines[..lower]
        .iter()
        .rposition(|s| s.starts_with("──"))
        .unwrap();
    (upper, lower, lines[lower].chars().count())
}

// Synthetic: replace only the actual composer, retaining captured history/styles.
fn draft(text: &str, body: &str) -> String {
    let (upper, lower, _) = bounds(text);
    let mut lines: Vec<_> = text.lines().map(str::to_owned).collect();
    lines.splice(
        upper + 1..lower,
        body.split('\n').map(|s| format!("\x1b[0m{s}")),
    );
    lines.join("\n")
}

// Captured: pi 1.5.0, 2026-11-15, sealed private tmux capture-pane -p -e -N.
// E2's two refused frames were byte-identical; README maps each fixture source.
#[test]
fn captured_update_notice_is_not_an_editor_border() {
    use FreshEditorEvidence::{Present, Unknown};
    for (text, freshness, conversation) in [
        (POST_TURN, Unknown, false),
        // The banner has scrolled off this real nonzero-usage conversation.
        (capture!("rehearsal", "1.5.0"), Unknown, true),
        (capture!("startup_224x16", "1.5.0"), Unknown, false),
        (capture!("startup_224x12", "1.5.0"), Unknown, false),
        (capture!("startup_120x16", "1.5.0"), Unknown, false),
        (capture!("startup_80x16", "1.5.0"), Unknown, false),
        (capture!("startup_60x20", "1.5.0"), Unknown, false),
        (capture!("startup_40x20", "1.5.0"), Unknown, false),
        (capture!("startup_fd_status", "1.5.0"), Unknown, false),
        (capture!("startup_224x48", "1.5.0"), Present, false),
        (STARTUP, Present, false),
    ] {
        let result = editor(text, InputState::Empty, TurnState::NotRunning);
        assert!(safe(&result));
        assert_eq!(result.facts.fresh_editor, freshness);
        assert_eq!(result.conversation, conversation);
    }
}

// Captured: pi 1.5.0, 2026-11-15, sealed private tmux capture-pane -p -e -N.
// Built-in themes, no input/prompt; bold title but neither label is faint.
#[test]
fn builtin_theme_update_notices_keep_editor_facts() {
    for text in [
        capture!("startup_dark", "1.5.0"),
        capture!("startup_light", "1.5.0"),
    ] {
        let decoded = StyledScreen::parse(text).unwrap();
        assert_eq!(
            decoded.bold_at(decoded.text().find("Update Available").unwrap()),
            Some(true)
        );
        for label in ["New version", "Changelog:"] {
            assert_eq!(
                decoded.faint_at(decoded.text().find(label).unwrap()),
                Some(false)
            );
        }
        let result = editor(text, InputState::Empty, TurnState::NotRunning);
        assert!(safe(&result));
        assert_eq!(result.facts.fresh_editor, FreshEditorEvidence::Present);
        assert!(!result.conversation);
        // Synthetic: a clipped header and multibyte history must not shift styles
        // or require startup-only history to recognize the same notice/editor.
        let clipped = format!(
            " Transcript 日本語 🦀\n\n{}",
            text.lines().skip(5).collect::<Vec<_>>().join("\n")
        );
        let result = editor(&clipped, InputState::Empty, TurnState::NotRunning);
        assert!(safe(&result));
        assert_eq!(result.facts.fresh_editor, FreshEditorEvidence::Unknown);
    }
}

#[test]
fn bold_notice_does_not_authorize_an_unsafe_editor() {
    for text in [
        capture!("startup_dark", "1.5.0"),
        capture!("startup_light", "1.5.0"),
    ] {
        // Synthetic: retain the real bold notice, change only the active editor.
        let result = editor(
            &draft(text, "do not clear this draft"),
            InputState::Draft,
            TurnState::NotRunning,
        );
        assert!(!safe(&result));
        let result = observe(&draft(
            text,
            "Choose a model\nselect to submit · esc cancel",
        ));
        assert!(!safe(&result));
        assert_ne!(result.facts.dialog, DialogState::None);
        let (upper, _, width) = bounds(text);
        let border = "─".repeat(width);
        let working = format!("── ⠋ Working {}", "─".repeat(width - 13));
        let mut lines: Vec<_> = text.lines().map(str::to_owned).collect();
        lines[upper] = lines[upper].replace(&border, &working);
        let result = editor(&lines.join("\n"), InputState::Empty, TurnState::Running);
        assert!(!safe(&result));
        let pasted_rule = draft(text, &format!("draft\n{border}\n"));
        assert_eq!(observe(&pasted_rule).facts, Default::default());
    }
}

#[test]
fn notice_requires_faint_labels_or_a_bold_title() {
    // Synthetic: faint labels still prove the captured notice without bold.
    let faint_only = POST_TURN.replace("\x1b[1m", "\x1b[22m");
    assert_ne!(faint_only, POST_TURN);
    assert!(safe(&editor(
        &faint_only,
        InputState::Empty,
        TurnState::NotRunning
    )));
    for text in [
        capture!("startup_dark", "1.5.0"),
        capture!("startup_light", "1.5.0"),
    ] {
        // Synthetic: complete startup text is not a Styled attribute fallback.
        for changed in [
            text.replace("\x1b[1m", "\x1b[22m"),
            text.replace("Update Available", "Other notice"),
            text.replace(" is available. Run ", " is released. Run "),
            text.replace("Changelog:", "Release log:"),
            // A single bold glyph is not the captured bold title.
            text.replace("Update Available", "U\x1b[22mpdate Available"),
        ] {
            assert_ne!(changed, text);
            assert_eq!(observe(&changed).facts, Default::default());
            assert_eq!(observe(&changed).dialog, ControlDialog::Unknown);
        }
    }
}

// Captured: pi 1.5.0, 2026-11-15, sealed private tmux offline/resource startups.
// Captured: pi 1.5.2, 2026-11-15, sealed private tmux offline/resource startups.
// Both: wide/narrow capture-pane -p -e -N; no input or prompt.
#[test]
fn ordinary_startup_stays_ready_at_both_versions() {
    for text in [
        capture!("offline-wide", "1.5.0"),
        capture!("offline-narrow", "1.5.0"),
        capture!("resources-wide", "1.5.0"),
        capture!("resources-narrow", "1.5.0"),
        capture!("offline-wide", "1.5.2"),
        capture!("offline-narrow", "1.5.2"),
        capture!("resources-wide", "1.5.2"),
        capture!("resources-narrow", "1.5.2"),
    ] {
        let result = editor(text, InputState::Empty, TurnState::NotRunning);
        assert!(safe(&result));
        assert!(!result.conversation);
    }
}

// Captured: pi 1.5.0, 2026-11-15, real atmd clear in sealed private tmux.
// Captured: pi 1.5.2, 2026-11-15, real atmd clear in sealed private tmux.
// Before/after one guarded /new per version; no prompt or direct rig input.
#[test]
fn real_daemon_clear_frames_stay_safe_at_both_versions() {
    for text in [
        capture!("atmd_before", "1.5.0"),
        capture!("atmd_after", "1.5.0"),
        capture!("atmd_before", "1.5.2"),
        capture!("atmd_after", "1.5.2"),
    ] {
        let result = editor(text, InputState::Empty, TurnState::NotRunning);
        assert!(safe(&result));
        assert!(!result.conversation);
    }
}

#[test]
fn staged_new_under_notice_is_exact() {
    // Synthetic: the actual composer holds the command, not a history match.
    for command in ["/new", " /new", "/newx", "/wrong"] {
        let text = draft(POST_TURN, command);
        let result = editor(&text, InputState::Draft, TurnState::NotRunning);
        assert!(!safe(&result));
        assert_eq!(
            staged_draft_matches(ScreenInput::Styled(&text), "/new"),
            Some(command == "/new")
        );
    }
}

#[test]
fn update_notice_does_not_hide_an_unsafe_editor() {
    // Captured: pi 1.5.0, 2026-11-14, P1 private tmux capture-pane -p -e -N.
    let real = [
        include_str!("fixtures/screen_styled_p1_C4-one-draft-120x40_1.5.0.txt"),
        include_str!("fixtures/screen_styled_p1_C1-multi-pasted-120x40_1.5.0.txt"),
        include_str!("fixtures/screen_styled_p1_C2-steer-one-held-120x40_1.5.0.txt"),
        include_str!("fixtures/screen_styled_p1_C3-one-selector-120x40_1.5.0.txt"),
    ];
    for labelled in real {
        assert!(!safe(&observe(labelled.split_once('\n').unwrap().1)));
    }
    // Synthetic: a real selector's editor-slot body under the E2 startup notice.
    let selector = real[3].split_once('\n').unwrap().1;
    let (upper, lower, _) = bounds(selector);
    let dialog = selector
        .lines()
        .skip(upper + 1)
        .take(lower - upper - 1)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!safe(&observe(&draft(POST_TURN, &dialog))));
    assert!(!safe(&observe(&draft(
        POST_TURN,
        "do not clear this draft"
    ))));
    // Synthetic: Working upper border, with the same notice and blank composer.
    let (upper, _, width) = bounds(POST_TURN);
    let border = "─".repeat(width);
    let working = format!("── ⠋ Working {}", "─".repeat(width - 13));
    assert_eq!(working.chars().count(), width);
    let mut lines: Vec<_> = POST_TURN.lines().map(str::to_owned).collect();
    lines[upper] = lines[upper].replace(&border, &working);
    let running = lines.join("\n");
    let result = editor(&running, InputState::Empty, TurnState::Running);
    assert!(!safe(&result));
    assert_eq!(
        observe(&running.replace("Working", "Waiting")).facts,
        Default::default()
    );
}

#[test]
fn every_prior_rule_needs_notice_proof() {
    // Synthetic: the valid notice must not excuse an extra pasted editor rule.
    let (_, _, width) = bounds(POST_TURN);
    let border = "─".repeat(width);
    for body in [format!("draft\n{border}\n"), format!("draft\n\n{border}\n")] {
        assert_eq!(observe(&draft(POST_TURN, &body)).facts, Default::default());
    }
    // Synthetic: actual editor upper border imitates the top of a copied notice.
    // Its text is nonfaint and nonbold. Text-only notice proof is unsafe.
    let body = format!(" Update Available\n New version 1.5.2 is available. Run pi update\n Changelog: https://pi.dev/changelog\n{border}\n\n{border}\n");
    let counterfeit = draft(STARTUP, &body);
    assert_eq!(observe(&counterfeit).facts, Default::default());
}

#[test]
fn notice_does_not_hide_an_earlier_working_border() {
    // Synthetic: prepend a real 120-column notice to the captured Working editor
    // and insert a queued draft, nonfaint full-width rule and cursor/staged row.
    // This is a parser refusal probe, not proof that live paste can render it.
    let running = include_str!("fixtures/screen_styled_p1_C2-steer-one-held-120x40_1.5.0.txt")
        .split_once('\n')
        .unwrap()
        .1;
    editor(running, InputState::Empty, TurnState::Running);
    let startup = capture!("startup_120x16", "1.5.0");
    let decoded = plain(startup);
    let rules: Vec<_> = decoded
        .lines()
        .enumerate()
        .filter(|(_, row)| row.starts_with("──"))
        .map(|(i, _)| i)
        .collect();
    let notice = startup
        .lines()
        .skip(rules[0])
        .take(rules[1] - rules[0] + 1)
        .collect::<Vec<_>>()
        .join("\n");
    let (_, _, width) = bounds(running);
    assert_eq!(width, 120);
    let border = "─".repeat(width);
    let results = ["", "MSG"].map(|staged| {
        let without_notice = draft(
            running,
            &format!("keep this unfinished\n{border}\n{staged}"),
        );
        // Observe, but do not require a fix for, deferred notice-free crrv-C M1.
        let control = observe(&without_notice);
        eprintln!(
            "notice-free {staged:?}: {:?}; staged {:?}",
            control.facts,
            staged_draft_matches(ScreenInput::Styled(&without_notice), "MSG")
        );
        let with_notice = format!("{notice}\n\n{without_notice}");
        let result = observe(&with_notice);
        let matched = staged_draft_matches(ScreenInput::Styled(&with_notice), "MSG");
        eprintln!(
            "notice-bearing {staged:?}: {:?}; staged {matched:?}",
            result.facts
        );
        (result, matched)
    });
    for (result, matched) in results {
        assert_eq!(result.facts, Default::default());
        assert_eq!(result.dialog, ControlDialog::Unknown);
        assert_eq!(matched, None);
    }
}

#[test]
fn damaged_notice_or_editor_stays_unknown() {
    // Synthetic: broken chrome, missing both style proofs or unsupported control.
    for changed in [
        POST_TURN.replace("Update Available", "Other notice"),
        POST_TURN.replace(" is available. Run ", " is released. Run "),
        POST_TURN.replace("Changelog: ", "Release log: "),
        POST_TURN
            .replace("\x1b[2mNew version", "\x1b[22mNew version")
            .replace("\x1b[1m", "\x1b[22m"),
        POST_TURN
            .replace("\x1b[2mChangelog:", "\x1b[22mChangelog:")
            .replace("\x1b[1m", "\x1b[22m"),
        POST_TURN.replace("0.7%/500k", "unknown footer"),
        format!("{POST_TURN}\x1b[8m"),
        format!("{POST_TURN}\x1b["),
        format!("{POST_TURN}\nshell$ "),
        POST_TURN.replace(" \x1b[2mChangelog:", " release note\n\n \x1b[2mChangelog:"),
    ] {
        assert_ne!(changed, POST_TURN);
        assert_eq!(observe(&changed).facts, Default::default());
        assert_eq!(observe(&changed).dialog, ControlDialog::Unknown);
    }
    // Synthetic: delete either notice bound, or the actual editor upper bound.
    let p = plain(POST_TURN);
    let rules: Vec<_> = p
        .lines()
        .enumerate()
        .filter(|(_, s)| s.trim().starts_with("──"))
        .map(|(i, _)| i)
        .collect();
    for index in [rules[0], rules[1], bounds(POST_TURN).0] {
        let mut lines: Vec<_> = POST_TURN.lines().collect();
        lines.remove(index);
        assert_eq!(observe(&lines.join("\n")).facts, Default::default());
    }
}

#[test]
fn values_and_transcript_length_do_not_change_editor_permission() {
    // Synthetic: variable values, Unicode byte offsets and longer history.
    let (upper, _, _) = bounds(POST_TURN);
    let mut longer: Vec<_> = POST_TURN.lines().map(str::to_owned).collect();
    longer.splice(upper..upper, (0..100).map(|n| format!(" Transcript {n}\n")));
    for changed in [
        POST_TURN.replace("New version 1.5.2", "New version 12.34.567"),
        POST_TURN.replace("Warning: tmux", "Warning: 日本語 tmux"),
        format!(" A prefixed transcript 🦀\n\n{POST_TURN}"),
        longer.join("\n"),
    ] {
        let result = editor(&changed, InputState::Empty, TurnState::NotRunning);
        assert!(safe(&result));
        assert_eq!(result.facts.fresh_editor, FreshEditorEvidence::Unknown);
    }
    // Plain has no new notice-style proof; retain its previous refusal here.
    assert_eq!(
        observe_control_screen(ScreenInput::Plain(&plain(POST_TURN)), None).facts,
        Default::default()
    );
}

// Captured: pi 1.5.0, 2026-11-15, private offline startup with run-dir
// lastChangelogVersion=1.4.9 and collapseChangelog true/false; no input.
#[test]
fn already_unknown_changelogs_remain_refused() {
    for text in [
        capture!("changelog-condensed-wide", "1.5.0"),
        capture!("changelog-condensed-narrow", "1.5.0"),
        capture!("changelog-detailed-narrow", "1.5.0"),
    ] {
        let result = observe(text);
        assert_eq!(result.facts, Default::default());
        assert_eq!(result.dialog, ControlDialog::Unknown);
        assert!(!safe(&result));
    }
}
