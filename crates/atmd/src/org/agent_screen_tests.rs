//! Table tests for the per-harness screen dispatch. No I/O.
//!
//! Screens are the adapters' real styled captures (each adapter's fixture
//! README records its `Captured:` line); process facts are hand-written.

use super::agent_screen::*;
use crate::control::reservation::{CaptureFormat, PaneCapture, PaneInspection};
use atm_core::harness_control::{ControlDialog, ControlScreen, HookListEntry};
use atm_core::{DialogState, Harness, InputState, ScreenFacts, ScreenInput, TurnState};

pub(super) const ROOT: u32 = 4100;

const CLAUDE_FRESH: &str = include_str!(
    "../../../atm-claude-adapter/tests/fixtures/screen_styled_u5b_rich-enabled-fresh_2.1.325.txt"
);
const CLAUDE_DRAFT: &str = include_str!(
    "../../../atm-claude-adapter/tests/fixtures/screen_styled_p1_B-auto-pasted-120x40_2.1.325.txt"
);
const CODEX_FRESH: &str = include_str!(
    "../../../atm-codex-adapter/tests/fixtures/screen_styled_u5b_rich-fresh_0.167.0.txt"
);
const CODEX_DRAFT: &str = include_str!(
    "../../../atm-codex-adapter/tests/fixtures/screen_styled_p1_c03-draft-before-100x24_0.167.0.txt"
);
const PI_FRESH: &str = include_str!(
    "../../../atm-pi-adapter/tests/fixtures/screen_styled_p1_B-fresh-120x40_1.5.0.txt"
);
const PI_DRAFT: &str = include_str!(
    "../../../atm-pi-adapter/tests/fixtures/screen_styled_p1_B-0-event-pasted-120x40_1.5.0.txt"
);

type Observe = fn(ScreenInput<'_>, Option<atm_core::ScreenCursor>) -> ScreenFacts;

/// (harness, its foreground name, a fresh editor, a staged draft and its text,
/// the adapter's own observer)
const CASES: [(Harness, &str, &str, &str, &str, Observe); 3] = [
    (
        Harness::ClaudeCode,
        "claude",
        CLAUDE_FRESH,
        CLAUDE_DRAFT,
        "B_AUTO_EVENT_SENTINEL",
        atm_claude_adapter::screen::observe_screen,
    ),
    (
        Harness::Codex,
        "codex",
        CODEX_FRESH,
        CODEX_DRAFT,
        "C_EXISTING_DRAFT:",
        atm_codex_adapter::screen::observe_screen,
    ),
    (
        Harness::Pi,
        "pi",
        PI_FRESH,
        PI_DRAFT,
        "B_SENTINEL_0_event",
        atm_pi_adapter::screen::observe_screen,
    ),
];

/// A fixture without its leading `Captured:` provenance line: what tmux returned.
pub(super) fn body(fixture: &str) -> &str {
    match fixture.split_once('\n') {
        Some((first, rest)) if first.starts_with("Captured:") => rest,
        _ => fixture,
    }
}

pub(super) fn capture(
    text: &str,
    format: CaptureFormat,
    root: Option<u32>,
    command: Option<&str>,
) -> PaneCapture {
    let text = body(text);
    let inspection = PaneInspection::new(
        atm_tmux::PaneInfo {
            pane_id: "%1".into(),
            session_name: "private".into(),
            window_index: 0,
            pane_pid: root.unwrap_or(0),
            width: 120,
            height: text.lines().count() as u16,
            is_active: false,
        },
        Some("p/lead".into()),
        false,
        false,
        Some(true),
        0,
        0,
    )
    .with_foreground_process(None, command.map(Into::into));
    PaneCapture::new(text.into(), inspection, format)
}

pub(super) fn pinned(command: &str) -> PaneBaseline {
    PaneBaseline::shell(ROOT, "zsh".into())
        .with_harness(command.into())
        .unwrap()
}

/// The observations that must refuse for a baseline pinned on (ROOT, zsh, name):
/// (root pid, command, the verdict).
fn refusals() -> [(Option<u32>, Option<&'static str>, Option<Foreground>); 5] {
    [
        (None, Some("zsh"), None),
        (Some(ROOT + 1), Some("zsh"), None),
        (Some(ROOT), None, None),
        (Some(ROOT), Some("zsh"), Some(Foreground::Shell)),
        (Some(ROOT), Some("node"), None),
    ]
}

#[test]
fn fixtures_are_read_without_their_provenance_line() {
    for (_, _, fresh, draft, _, _) in CASES {
        for fixture in [fresh, draft] {
            assert!(fixture.starts_with("Captured:"));
            assert!(!body(fixture).starts_with("Captured:"));
            assert_eq!(body(body(fixture)), body(fixture));
        }
        // The staged-draft fixtures are full `-N` captures with tmux's padding.
        assert!(body(draft).lines().any(|line| line.ends_with(' ')));
    }
}

#[test]
fn foreground_verdict_table() {
    let unproven = PaneBaseline::unproven();
    let shell = PaneBaseline::shell(ROOT, "zsh".into());
    let pinned = pinned("claude");
    let adopted = PaneBaseline::adopted(ROOT)
        .with_harness("claude".into())
        .unwrap();
    use Foreground::{Harness as H, Shell as S};
    for (name, baseline, root, command, expected) in [
        // Row 1: the root process is unknown, or not the recorded one.
        ("unproven", &unproven, Some(ROOT), Some("claude"), None),
        ("unproven, nothing seen", &unproven, None, None, None),
        ("root unknown", &pinned, None, Some("claude"), None),
        ("root reused", &pinned, Some(ROOT + 1), Some("claude"), None),
        (
            "root reused, shell name",
            &pinned,
            Some(ROOT + 1),
            Some("zsh"),
            None,
        ),
        // Row 2: the command is unknown.
        ("command unknown", &pinned, Some(ROOT), None, None),
        // Row 3: the recorded shell.
        ("shell", &pinned, Some(ROOT), Some("zsh"), Some(S)),
        (
            "shell, nothing pinned",
            &shell,
            Some(ROOT),
            Some("zsh"),
            Some(S),
        ),
        // Row 4: the pinned harness.
        ("harness", &pinned, Some(ROOT), Some("claude"), Some(H)),
        (
            "adopted harness",
            &adopted,
            Some(ROOT),
            Some("claude"),
            Some(H),
        ),
        // Row 5: anything else.
        ("another command", &pinned, Some(ROOT), Some("node"), None),
        ("nothing pinned", &shell, Some(ROOT), Some("claude"), None),
        ("case differs", &pinned, Some(ROOT), Some("Claude"), None),
        ("empty command", &pinned, Some(ROOT), Some(""), None),
        (
            "adopted, another command",
            &adopted,
            Some(ROOT),
            Some("zsh"),
            None,
        ),
    ] {
        assert_eq!(foreground(baseline, root, command), expected, "{name}");
    }
}

#[test]
fn verdict_adopted_root_harness_is_not_shell() {
    // A harness that is the pane's root process: no shell name was recorded, so
    // no command can read as Shell, the harness's own least of all.
    let baseline = PaneBaseline::adopted(ROOT)
        .with_harness("claude".into())
        .unwrap();
    for command in ["claude", "zsh", "bash", "sh"] {
        assert_ne!(
            foreground(&baseline, Some(ROOT), Some(command)),
            Some(Foreground::Shell),
            "{command}"
        );
    }
    assert_eq!(
        foreground(&baseline, Some(ROOT), Some("claude")),
        Some(Foreground::Harness)
    );
}

#[test]
fn with_harness_refuses_shell_name_and_repin() {
    let shell = PaneBaseline::shell(ROOT, "zsh".into());
    // A harness running under the shell's own name can't be told from the shell.
    assert_eq!(shell.clone().with_harness("zsh".into()), None);
    let pinned = shell.with_harness("node".into()).unwrap();
    // Never re-pinned: `node` stays, and `pi` then refuses.
    assert_eq!(pinned.clone().with_harness("pi".into()), None);
    assert_eq!(pinned.clone().with_harness("node".into()), Some(pinned));
    assert_eq!(PaneBaseline::unproven().with_harness("claude".into()), None);
    assert!(PaneBaseline::adopted(ROOT)
        .with_harness("zsh".into())
        .is_some());
}

#[test]
fn dispatch_covers_every_adapter_and_refuses_the_rest() {
    // `Harness` is non-exhaustive, so this follows the registry instead of a
    // match: discovery is enabled exactly for the harnesses that have an
    // adapter, and enabling it for a new one fails here until it has a screen.
    let mut adapters = Vec::new();
    for definition in atm_core::builtin_harnesses() {
        assert_eq!(
            agent_screen(definition.harness).is_some(),
            definition.discovery_enabled,
            "{:?}",
            definition.harness
        );
        if definition.discovery_enabled {
            adapters.push(definition.harness);
        }
    }
    adapters.sort_by_key(|harness| harness.short_tag());
    assert_eq!(adapters, [Harness::ClaudeCode, Harness::Codex, Harness::Pi]);
    assert!(agent_screen(Harness::Unknown).is_none());
}

#[test]
fn capture_format_follows_adapter_policy() {
    for (harness, policy) in [
        (
            Harness::ClaudeCode,
            atm_claude_adapter::screen::CONTROL_POLICY,
        ),
        (Harness::Pi, atm_pi_adapter::screen::CONTROL_POLICY),
        (Harness::Codex, atm_codex_adapter::screen::CONTROL_POLICY),
    ] {
        let screen = agent_screen(harness).unwrap();
        assert_eq!(*screen.policy(), policy, "{harness:?}");
        assert_eq!(
            screen.capture_format(),
            match policy.capture {
                atm_core::harness_control::ScreenTextFormat::Styled => CaptureFormat::Ansi,
                atm_core::harness_control::ScreenTextFormat::Plain => CaptureFormat::Plain,
            },
            "{harness:?}"
        );
    }
}

#[test]
fn facts_are_unknown_unless_harness_is_foreground() {
    for (harness, name, fresh, _, _, observe) in CASES {
        let screen = agent_screen(harness).unwrap();
        let baseline = pinned(name);
        // The positive twin: the same frame with the harness in front.
        let facts = screen.facts(
            &capture(fresh, CaptureFormat::Ansi, Some(ROOT), Some(name)),
            &baseline,
        );
        // Exactly the adapter's read of the styled capture, which sees the
        // empty editor. (Claude's legacy observer leaves the turn Unknown on a
        // fresh screen; its control read, below, does not.)
        assert_eq!(
            facts,
            observe(ScreenInput::Styled(body(fresh)), None),
            "{harness:?}"
        );
        assert_eq!(facts.input, InputState::Empty, "{harness:?}");
        assert_eq!(facts.dialog, DialogState::None, "{harness:?}");
        for (root, command, verdict) in refusals() {
            let frame = capture(fresh, CaptureFormat::Ansi, root, command);
            assert_eq!(screen.foreground(&frame, &baseline), verdict);
            assert_eq!(
                screen.facts(&frame, &baseline),
                ScreenFacts::default(),
                "{harness:?} {root:?} {command:?}"
            );
        }
        assert_eq!(
            screen.facts(
                &capture(fresh, CaptureFormat::Ansi, Some(ROOT), Some(name)),
                &PaneBaseline::unproven()
            ),
            ScreenFacts::default()
        );
    }
}

/// Real idle Claude editors in manual mode, which the adapter's legacy
/// observer reads all Unknown: three launches and one after a conversation.
const CLAUDE_MANUAL_IDLE: [&str; 4] = [
    include_str!(
        "../../../atm-claude-adapter/tests/fixtures/screen_control_p1_A80-ready-80x40_2.1.325.txt"
    ),
    include_str!(
        "../../../atm-claude-adapter/tests/fixtures/screen_control_p1_A120-ready-120x40_2.1.325.txt"
    ),
    include_str!(
        "../../../atm-claude-adapter/tests/fixtures/screen_control_p1_A200-ready-200x40_2.1.325.txt"
    ),
    include_str!(
        "../../../atm-claude-adapter/tests/fixtures/screen_control_p1_B-baseline-conversation-120x40_2.1.325.txt"
    ),
];

#[test]
fn facts_read_real_idle_claude_editors_as_the_control_read_does() {
    let screen = agent_screen(Harness::ClaudeCode).unwrap();
    let baseline = pinned("claude");
    for fixture in CLAUDE_MANUAL_IDLE {
        let frame = capture(fixture, CaptureFormat::Ansi, Some(ROOT), Some("claude"));
        let facts = screen.facts(&frame, &baseline);
        assert_eq!(facts, screen.control(&frame, &baseline).facts);
        assert_eq!(facts.input, InputState::Empty);
        assert_eq!(facts.turn, TurnState::NotRunning);
        assert_eq!(facts.dialog, DialogState::None);
        // Synthetic: a shell prompt below the same frame. The editor above it
        // is history now, so nothing may be read from it.
        let mutated = format!("{}\nshell$ ", body(fixture));
        let frame = capture(&mutated, CaptureFormat::Ansi, Some(ROOT), Some("claude"));
        assert_eq!(screen.facts(&frame, &baseline), ScreenFacts::default());
    }
}

#[test]
fn staged_is_none_unless_harness_is_foreground() {
    for (harness, name, _, draft, text, _) in CASES {
        let screen = agent_screen(harness).unwrap();
        let baseline = pinned(name);
        let front = capture(draft, CaptureFormat::Ansi, Some(ROOT), Some(name));
        assert_eq!(
            screen.staged(&front, &baseline, text),
            Some(true),
            "{harness:?}"
        );
        assert_eq!(
            screen.staged(&front, &baseline, &format!(" {text}")),
            Some(false),
            "{harness:?}: leading whitespace is part of the match"
        );
        for (root, command, _) in refusals() {
            assert_eq!(
                screen.staged(
                    &capture(draft, CaptureFormat::Ansi, root, command),
                    &baseline,
                    text
                ),
                None,
                "{harness:?} {root:?} {command:?}"
            );
        }
    }
}

#[test]
fn control_is_unknown_unless_harness_is_foreground() {
    for (harness, name, fresh, _, _, _) in CASES {
        let screen = agent_screen(harness).unwrap();
        let baseline = pinned(name);
        let control = screen.control(
            &capture(fresh, CaptureFormat::Ansi, Some(ROOT), Some(name)),
            &baseline,
        );
        assert_eq!(control.dialog, ControlDialog::None, "{harness:?}");
        assert_eq!(control.facts.input, InputState::Empty, "{harness:?}");
        assert_eq!(
            control.facts.turn,
            atm_core::TurnState::NotRunning,
            "{harness:?}"
        );
        for (root, command, _) in refusals() {
            assert_eq!(
                screen.control(
                    &capture(fresh, CaptureFormat::Ansi, root, command),
                    &baseline
                ),
                ControlScreen::default(),
                "{harness:?} {root:?} {command:?}"
            );
        }
    }
}

#[test]
fn wrong_capture_format_is_unknown() {
    for (harness, name, fresh, draft, text, _) in CASES {
        let screen = agent_screen(harness).unwrap();
        let baseline = pinned(name);
        // The same bytes, declared as a plain capture: the adapter asked for styled.
        let plain = capture(fresh, CaptureFormat::Plain, Some(ROOT), Some(name));
        assert_eq!(screen.facts(&plain, &baseline), ScreenFacts::default());
        assert_eq!(screen.control(&plain, &baseline), ControlScreen::default());
        assert_eq!(
            screen.staged(
                &capture(draft, CaptureFormat::Plain, Some(ROOT), Some(name)),
                &baseline,
                text
            ),
            None,
            "{harness:?}"
        );
        assert_eq!(
            screen.start_candidate(
                &capture(fresh, CaptureFormat::Plain, Some(ROOT), Some(name)),
                &PaneBaseline::shell(ROOT, "zsh".into())
            ),
            None,
            "{harness:?}"
        );
    }
}

#[test]
fn start_candidate_table() {
    for (harness, name, fresh, _, _, _) in CASES {
        let screen = agent_screen(harness).unwrap();
        let shell = PaneBaseline::shell(ROOT, "zsh".into());
        let frame = |root, command| capture(fresh, CaptureFormat::Ansi, root, command);
        // The shell in front (right after the launch Enter, or a harness that
        // exited under a stale frame): nothing is read.
        assert_eq!(
            screen.start_candidate(&frame(Some(ROOT), Some("zsh")), &shell),
            None
        );
        assert_eq!(
            screen.start_candidate(&frame(Some(ROOT), None), &shell),
            None
        );
        assert_eq!(
            screen.start_candidate(&frame(None, Some(name)), &shell),
            None
        );
        assert_eq!(
            screen.start_candidate(&frame(Some(ROOT + 1), Some(name)), &shell),
            None
        );
        assert_eq!(
            screen.start_candidate(&frame(Some(ROOT), Some(name)), &PaneBaseline::unproven()),
            None
        );
        // Once pinned, the masked reads apply instead.
        assert_eq!(
            screen.start_candidate(&frame(Some(ROOT), Some(name)), &pinned(name)),
            None
        );
        // Another command: its name and the adapter's read, for the caller to judge.
        for baseline in [&shell, &PaneBaseline::adopted(ROOT)] {
            let (command, control) = screen
                .start_candidate(&frame(Some(ROOT), Some(name)), baseline)
                .unwrap();
            assert_eq!(command, name);
            assert_eq!(control.dialog, ControlDialog::None, "{harness:?}");
        }
        // A command that is not the shell's with a screen no adapter knows (the
        // `node` phase of an npm launch): the read is Unknown, so nothing pins.
        let (command, control) = screen
            .start_candidate(
                &capture("$ codex\n", CaptureFormat::Ansi, Some(ROOT), Some("node")),
                &shell,
            )
            .unwrap();
        assert_eq!(command, "node");
        assert_eq!(control, ControlScreen::default());
        // Until a harness is pinned, the masked reads stay Unknown.
        assert_eq!(
            screen.facts(&frame(Some(ROOT), Some(name)), &shell),
            ScreenFacts::default()
        );
    }
}

#[test]
fn hooks_valid_is_false_without_a_hook_list() {
    let entries = [HookListEntry::new("SessionStart".into(), 1, 1, None)];
    for harness in [Harness::ClaudeCode, Harness::Pi] {
        assert!(!agent_screen(harness).unwrap().hooks_valid(&entries));
        assert!(!agent_screen(harness).unwrap().hooks_valid(&[]));
    }
    // Codex's own validator decides; an empty list is not atm's eleven rows.
    assert!(!agent_screen(Harness::Codex).unwrap().hooks_valid(&[]));
}
