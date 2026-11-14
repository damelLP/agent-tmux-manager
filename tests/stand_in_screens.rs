//! Pane/adapter contract: expected facts are pinned independently of rendering.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/stand_in.rs"]
mod stand_in;

use atm_core::{DialogState, FreshEditorEvidence, InputState, ScreenFacts, TurnState};
use serde::Deserialize;
use stand_in::{StandInConfig, StandInMode, StandInPane, StandInRig};

#[derive(Debug, Deserialize)]
struct Source {
    key: String,
    mode: String,
    source: String,
    columns: u16,
    rows: u16,
    style: Option<String>,
}

fn sources() -> Vec<Source> {
    serde_json::from_str(include_str!("support/stand_in/screens.json")).unwrap()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

fn body(text: &str) -> &str {
    if text.starts_with("Captured:") {
        text.split_once('\n').unwrap().1
    } else {
        text
    }
}

fn normalize(text: &str) -> String {
    // capture-pane -p discards trailing ASCII cells. Never normalize NBSP or
    // internal blank rows: those can change editor/draft interpretation.
    text.lines()
        .map(|line| line.trim_end_matches(' '))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end_matches('\n')
        .to_owned()
}

fn observe(mode: StandInMode, text: &str) -> ScreenFacts {
    let text = atm_core::ScreenInput::Plain(text);
    match mode {
        StandInMode::Claude => atm_claude_adapter::screen::observe_screen(text, None),
        StandInMode::Codex => atm_codex_adapter::screen::observe_screen(text, None),
        StandInMode::Pi => atm_pi_adapter::screen::observe_screen(text, None),
    }
}

fn facts(
    input: InputState,
    turn: TurnState,
    dialog: DialogState,
    fresh: FreshEditorEvidence,
) -> ScreenFacts {
    let mut result = ScreenFacts::default();
    result.input = input;
    result.turn = turn;
    result.dialog = dialog;
    result.fresh_editor = fresh;
    result
}

fn expected(key: &str) -> ScreenFacts {
    use DialogState::{None, Other, Permission, Unknown as DialogUnknown};
    use FreshEditorEvidence::{Present, Unknown as NotFresh};
    use InputState::{Draft, Empty, Unknown};
    use TurnState::{NotRunning, Running, Unknown as TurnUnknown};
    // U5/U5b expectations transcribed from the adapters' captured_screens tests.
    // P1 modal expectations are deliberately conservative under unchanged U8a.
    let (input, turn, dialog, fresh) = match key {
        "claude.fresh.rich-enabled" | "claude.fresh.u5" | "claude.fresh.restart"
        | "claude.draft-identical.poor"
        | "claude.erased.rich-enabled" => (Unknown, NotRunning, None, Present),
        "claude.fresh.poor" | "claude.erased.poor" | "claude.clear-complete.rich-enabled"
        | "claude.clear-complete.poor" | "claude.clear-complete.u5"
        | "pi.fresh.u5" | "pi.fresh.notice" | "pi.clear-complete.u5" => (Empty, NotRunning, None, Present),
        "claude.draft.rich-enabled" | "claude.draft.poor" | "claude.interrupted.u5"
        | "pi.draft.p1" | "pi.draft.p1-multi" => (Draft, NotRunning, None, NotFresh),
        "claude.working.u5" | "claude.clearing.u5" | "pi.working.u5"
        | "pi.working.coordinator-1" | "pi.working.coordinator-2" => (Empty, Running, None, NotFresh),
        "claude.idle.u5" | "claude.idle.after-turn" | "claude.denied.u5"
        | "pi.idle.u5" | "pi.idle.after-turn" | "pi.clearing.u5" | "pi.interrupted.u5"
        | "pi.idle.coordinator-1" | "pi.idle.coordinator-2" => (Empty, NotRunning, None, NotFresh),
        "claude.permission.u5" | "claude.permission.denial-pending" => (Unknown, TurnUnknown, Permission, NotFresh),
        "claude.folder-trust.u5" => (Unknown, TurnUnknown, DialogState::FolderTrust {
            folder: "/tmp/u8a-fixture/cwd/claude".into(),
        }, NotFresh),
        "claude.folder-trust.p1-80" | "claude.folder-trust.p1-120" | "claude.folder-trust.p1-200" =>
            (Unknown, TurnUnknown, DialogState::FolderTrust { folder: "/fixture/xxxxxxxxxxxxxxxxxxx/cwd".into() }, NotFresh),
        "codex.fresh.rich" | "codex.fresh.poor" | "codex.fresh.u5"
        | "codex.erased.rich" | "codex.erased.poor" | "codex.clear-complete.rich"
        | "codex.clear-complete.poor" | "codex.clear-complete.u5" => (Unknown, NotRunning, None, Present),
        "codex.working.u5" => (Unknown, Running, None, NotFresh),
        "codex.idle.u5" | "codex.idle.after-turn" | "codex.interrupted.u5" => (Unknown, NotRunning, None, NotFresh),
        "codex.clear-menu.u5" => (Draft, NotRunning, Other, NotFresh),
        "codex.update.u5" | "codex.update.restart" => (Unknown, TurnUnknown, Other, NotFresh),
        "claude.draft-identical.rich-enabled" // Separate update/focus-warning footer: U5b unsupported case.
        | "claude.shell.u5" | "claude.shell.final" | "codex.shell.u5" | "codex.shell.final"
        | "pi.shell.u5" | "pi.shell.final" | "codex.draft.rich" | "codex.draft.poor"
        | "codex.draft-identical.rich" | "codex.draft-identical.poor"
        | "claude.folder-trust-selected.p1-80" | "claude.folder-trust-selected.p1-120"
        | "claude.folder-trust-selected.p1-200" | "codex.folder-trust.p1"
        | "codex.hooks-review.p1" | "codex.hooks-list.p1" | "codex.worktree.p1"
        | "codex.worktree.p1-short" | "codex.permission.p1" | "codex.clearing.p1" =>
            (Unknown, TurnUnknown, DialogUnknown, NotFresh),
        _ => panic!("unreviewed source needs independent expected facts: {key}"),
    };
    facts(input, turn, dialog, fresh)
}

fn replay(rig: &StandInRig, pane: &StandInPane, source: &Source) -> String {
    let resized = rig.tmux(&[
        "resize-window",
        "-t",
        &pane.pane_id,
        "-x",
        &source.columns.to_string(),
        "-y",
        &source.rows.to_string(),
    ]);
    assert!(resized.status.success(), "{resized:?}");
    let after = rig.event_cursor(pane);
    let reply = rig.control(
        pane,
        serde_json::json!({"command":"render_source", "key": source.key}),
    );
    assert!(reply.get("error").is_none(), "{reply}");
    rig.wait_event(pane, after, |e| {
        e["kind"] == "rendered" && e["state"] == "source-replay" && e["source"] == source.source
    });
    rig.capture_plain(pane)
}

fn rendered(rig: &StandInRig, pane: &StandInPane, after: u64, frame: &str) {
    rig.wait_event(pane, after, |e| {
        e["kind"] == "rendered" && e["frame"] == frame
    });
}

fn assert_source(rig: &StandInRig, pane: &StandInPane, mode: StandInMode, key: &str) {
    let source = sources().into_iter().find(|s| s.key == key).unwrap();
    let original = read(&source.source);
    let captured = rig.capture_plain(pane);
    assert_eq!(
        normalize(&captured),
        normalize(body(&original)),
        "runtime source: {key}"
    );
    assert_eq!(
        observe(mode, &captured),
        expected(key),
        "runtime facts: {key}"
    );
}

#[test]
fn key_driven_lifecycle_matches_sources_and_repeated_clear_is_only_a_snapshot() {
    use DialogState::None as NoDialog;
    use FreshEditorEvidence::Unknown as NotFresh;
    use InputState::{Draft, Empty, Unknown};
    use TurnState::NotRunning;
    let mut rig = StandInRig::start();
    for mode in [StandInMode::Claude, StandInMode::Codex, StandInMode::Pi] {
        let mut config = StandInConfig::new(mode);
        config.manual_clock = true;
        let pane = rig.launch(&config);
        let name = mode.name();
        assert_source(&rig, &pane, mode, &format!("{name}.fresh.u5"));
        let after = rig.event_cursor(&pane);
        rig.key(&pane, "baseline draft 123");
        rig.wait_event(&pane, after, |e| {
            e["kind"] == "rendered" && e["draft"] == "baseline draft 123"
        });
        let want = if mode == StandInMode::Codex {
            ScreenFacts::default()
        } else {
            facts(Draft, NotRunning, NoDialog, NotFresh)
        };
        assert_eq!(
            observe(mode, &rig.capture_plain(&pane)),
            want,
            "typed draft: {name}"
        );
        let after = rig.event_cursor(&pane);
        rig.key(&pane, "Enter");
        rendered(&rig, &pane, after, "working");
        assert_source(&rig, &pane, mode, &format!("{name}.working.u5"));
        let after = rig.event_cursor(&pane);
        rig.control(
            &pane,
            serde_json::json!({"command":"advance_to", "now_ms":500}),
        );
        rendered(&rig, &pane, after, "idle");
        assert_source(&rig, &pane, mode, &format!("{name}.idle.u5"));
        let clear = if mode == StandInMode::Claude {
            "/clear"
        } else {
            "/new"
        };
        let after = rig.event_cursor(&pane);
        rig.key(&pane, clear);
        rig.wait_event(&pane, after, |e| {
            e["kind"] == "rendered" && e["draft"] == clear
        });
        if mode == StandInMode::Codex {
            assert_source(&rig, &pane, mode, "codex.clear-menu.u5");
        } else {
            assert_eq!(
                observe(mode, &rig.capture_plain(&pane)),
                facts(Draft, NotRunning, NoDialog, NotFresh)
            );
        }
        let after = rig.event_cursor(&pane);
        rig.key(&pane, "Enter");
        rendered(&rig, &pane, after, "clearing");
        if mode == StandInMode::Codex {
            assert_eq!(
                observe(mode, &rig.capture_plain(&pane)),
                ScreenFacts::default()
            );
        } else {
            assert_source(&rig, &pane, mode, &format!("{name}.clearing.u5"));
        }
        let after = rig.event_cursor(&pane);
        rig.control(
            &pane,
            serde_json::json!({"command":"advance_to", "now_ms":600}),
        );
        rendered(&rig, &pane, after, "clear-complete");
        assert_source(&rig, &pane, mode, &format!("{name}.clear-complete.u5"));
        let first_clear = rig.capture_plain(&pane);
        let after = rig.event_cursor(&pane);
        rig.paste(&pane, clear);
        rendered(&rig, &pane, after, "clearing");
        let after = rig.event_cursor(&pane);
        rig.control(
            &pane,
            serde_json::json!({"command":"advance_to", "now_ms":700}),
        );
        rendered(&rig, &pane, after, "clear-complete");
        assert_eq!(
            rig.capture_plain(&pane),
            first_clear,
            "repeated clear is not positive operation evidence: {name}"
        );
        let after = rig.event_cursor(&pane);
        rig.paste(&pane, "interrupted prompt");
        rendered(&rig, &pane, after, "working");
        let after = rig.event_cursor(&pane);
        rig.key(&pane, "Escape");
        rendered(&rig, &pane, after, "interrupted");
        let input = match mode {
            StandInMode::Claude => Draft,
            StandInMode::Codex => Unknown,
            StandInMode::Pi => Empty,
        };
        assert_eq!(
            observe(mode, &rig.capture_plain(&pane)),
            facts(input, NotRunning, NoDialog, NotFresh)
        );
        // Ctrl-U removes Claude's restored draft before sending the exit command.
        rig.key(&pane, "C-u");
        rig.paste(&pane, "/atm/exit");
        rig.wait_for_shell(&pane);
        assert_eq!(
            observe(mode, &rig.capture_plain(&pane)),
            ScreenFacts::default(),
            "actual shell: {name}"
        );
    }
    rig.close();
}

#[test]
fn lifecycle_facts_survive_documented_and_split_height_viewports() {
    let mut rig = StandInRig::start();
    for mode in [StandInMode::Codex, StandInMode::Pi] {
        let minimum_width = if mode == StandInMode::Pi { 86 } else { 80 };
        for (columns, rows) in [(100, 30), (120, 19), (minimum_width, 19)] {
            let mut config = StandInConfig::new(mode);
            config.manual_clock = true;
            config.columns = columns;
            config.rows = rows;
            let pane = rig.launch(&config);
            let check = |frame: &str| {
                let want = if mode == StandInMode::Codex && frame == "clearing" {
                    ScreenFacts::default()
                } else {
                    expected(&format!("{}.{frame}.u5", mode.name()))
                };
                assert_eq!(
                    observe(mode, &rig.capture_plain(&pane)),
                    want,
                    "{} {frame} {columns}x{rows}",
                    mode.name()
                );
            };
            check("fresh");
            let after = rig.event_cursor(&pane);
            rig.paste(&pane, "held work");
            rendered(&rig, &pane, after, "working");
            check("working");
            let after = rig.event_cursor(&pane);
            rig.control(
                &pane,
                serde_json::json!({"command":"advance_to", "now_ms":500}),
            );
            rendered(&rig, &pane, after, "idle");
            check("idle");
            let after = rig.event_cursor(&pane);
            rig.paste(&pane, "/new");
            rendered(&rig, &pane, after, "clearing");
            check("clearing");
            let after = rig.event_cursor(&pane);
            rig.control(
                &pane,
                serde_json::json!({"command":"advance_to", "now_ms":600}),
            );
            rendered(&rig, &pane, after, "clear-complete");
            check("clear-complete");
            let after = rig.event_cursor(&pane);
            rig.paste(&pane, "interrupt me");
            rendered(&rig, &pane, after, "working");
            let after = rig.event_cursor(&pane);
            rig.key(&pane, "Escape");
            rendered(&rig, &pane, after, "interrupted");
            check("interrupted");
        }
    }
    rig.close();
}

#[test]
fn value_only_variants_keep_lifecycle_facts_without_pinning_literals() {
    let mut rig = StandInRig::start();
    for (mode, version, model) in [
        (StandInMode::Claude, "9.8.765", "Opus 9.99"),
        (StandInMode::Codex, "0.987.6", "GPT-9-Dawn"),
        (StandInMode::Pi, "9.8.7", "gpt-9-dawn"),
    ] {
        let mut config = StandInConfig::new(mode);
        config.manual_clock = true;
        config.screen_variant = "values".into();
        let pane = rig.launch(&config);
        let text = rig.capture_plain(&pane);
        assert!(text.contains(version) && text.contains(model), "{text}");
        assert_eq!(
            observe(mode, &text),
            expected(&format!("{}.fresh.u5", mode.name()))
        );
        let after = rig.event_cursor(&pane);
        rig.paste(&pane, "held work");
        rendered(&rig, &pane, after, "working");
        assert_eq!(
            observe(mode, &rig.capture_plain(&pane)),
            expected(&format!("{}.working.u5", mode.name()))
        );
        let after = rig.event_cursor(&pane);
        rig.control(
            &pane,
            serde_json::json!({"command":"advance_to", "now_ms":500}),
        );
        rendered(&rig, &pane, after, "idle");
        let text = rig.capture_plain(&pane);
        assert_eq!(
            observe(mode, &text),
            expected(&format!("{}.idle.u5", mode.name()))
        );
        if mode == StandInMode::Pi {
            assert!(text.contains("$4.321"));
        }
    }
    rig.close();
}

#[test]
fn narrow_split_reports_minimum_through_the_rig() {
    let mut rig = StandInRig::start();
    for mode in [StandInMode::Codex, StandInMode::Pi] {
        let mut config = StandInConfig::new(mode);
        config.columns = 60;
        config.rows = 40;
        let failure =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.launch(&config)))
                .expect_err("60-column split must refuse misleading chrome");
        let message = failure.downcast_ref::<String>().unwrap();
        assert!(message.contains("requires at least"), "{message}");
        assert!(message.contains("60x40"), "{message}");
    }
    rig.close();
}

#[test]
fn typing_during_pi_work_and_codex_clear_keeps_the_process_and_draft() {
    let mut rig = StandInRig::start();
    for (mode, command, frame, deadline) in [
        (StandInMode::Pi, "held turn", "working", 500),
        (StandInMode::Codex, "/new", "clearing", 100),
    ] {
        let mut config = StandInConfig::new(mode);
        config.manual_clock = true;
        let pane = rig.launch(&config);
        let after = rig.event_cursor(&pane);
        rig.paste(&pane, command);
        rendered(&rig, &pane, after, frame);
        let after = rig.event_cursor(&pane);
        rig.key(&pane, "busy draft");
        rig.wait_event(&pane, after, |e| {
            e["kind"] == "rendered" && e["frame"] == frame && e["draft"] == "busy draft"
        });
        let text = rig.capture_plain(&pane);
        if mode == StandInMode::Pi {
            assert!(text.contains("busy draft"));
            assert_eq!(observe(mode, &text).turn, TurnState::Running);
        } else {
            assert!(text.trim().is_empty(), "clear gap has no editor: {text}");
        }
        let after = rig.event_cursor(&pane);
        rig.control(
            &pane,
            serde_json::json!({"command":"advance_to", "now_ms":deadline}),
        );
        rig.wait_event(&pane, after, |e| {
            e["kind"] == "rendered" && e["state"] == "idle" && e["draft"] == "busy draft"
        });
        assert!(rig.capture_plain(&pane).contains("busy draft"));
    }
    rig.close();
}

#[test]
fn native_placeholders_and_identical_drafts_never_manufacture_empty_input() {
    let mut rig = StandInRig::start();
    for (mode, variant, placeholder, want_draft) in [
        (
            StandInMode::Claude,
            "rich-enabled",
            "Try \"fix lint errors\"",
            expected("claude.fresh.rich-enabled"),
        ),
        (
            StandInMode::Codex,
            "rich",
            "Ask Codex to do anything",
            ScreenFacts::default(),
        ),
    ] {
        let mut config = StandInConfig::new(mode);
        config.columns = 100;
        config.rows = 30;
        config.screen_variant = variant.into();
        let pane = rig.launch(&config);
        assert_source(
            &rig,
            &pane,
            mode,
            &format!("{}.fresh.{variant}", mode.name()),
        );
        let after = rig.event_cursor(&pane);
        rig.key(&pane, placeholder);
        rig.wait_event(&pane, after, |e| {
            e["kind"] == "rendered" && e["draft"] == placeholder
        });
        assert_eq!(observe(mode, &rig.capture_plain(&pane)), want_draft);
        let after = rig.event_cursor(&pane);
        rig.key(&pane, "C-u");
        rig.wait_event(&pane, after, |e| {
            e["kind"] == "rendered" && e["draft"] == ""
        });
        assert_source(
            &rig,
            &pane,
            mode,
            &format!("{}.fresh.{variant}", mode.name()),
        );
    }
    let mut config = StandInConfig::new(StandInMode::Claude);
    config.columns = 100;
    config.rows = 30;
    config.screen_variant = "poor".into();
    let pane = rig.launch(&config);
    assert_source(&rig, &pane, StandInMode::Claude, "claude.fresh.poor");
    assert_eq!(
        observe(StandInMode::Claude, &rig.capture_plain(&pane)).input,
        InputState::Empty
    );
    rig.close();
}

#[test]
fn trust_permission_and_codex_dialog_keys_preserve_conservative_observer_facts() {
    let mut rig = StandInRig::start();
    let mut config = StandInConfig::new(StandInMode::Claude);
    config.folder_trust = true;
    let pane = rig.launch(&config);
    assert_source(
        &rig,
        &pane,
        StandInMode::Claude,
        "claude.folder-trust.p1-120",
    );
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "Down");
    rig.wait_event(&pane, after, |e| {
        e["kind"] == "rendered" && e["selection"] == 2
    });
    assert_source(
        &rig,
        &pane,
        StandInMode::Claude,
        "claude.folder-trust-selected.p1-120",
    );
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "Enter");
    rendered(&rig, &pane, after, "fresh");
    assert_source(&rig, &pane, StandInMode::Claude, "claude.fresh.u5");
    for cancel in [false, true] {
        let after = rig.event_cursor(&pane);
        rig.paste(&pane, "/atm/dialog");
        rendered(&rig, &pane, after, "permission");
        assert_source(&rig, &pane, StandInMode::Claude, "claude.permission.u5");
        if !cancel {
            let after = rig.event_cursor(&pane);
            rig.key(&pane, "Down");
            rig.wait_event(&pane, after, |e| {
                e["kind"] == "rendered" && e["selection"] == 2
            });
            assert_eq!(
                observe(StandInMode::Claude, &rig.capture_plain(&pane)),
                ScreenFacts::default()
            );
        }
        let after = rig.event_cursor(&pane);
        rig.key(&pane, if cancel { "Escape" } else { "Enter" });
        rendered(&rig, &pane, after, "denied");
        assert_source(&rig, &pane, StandInMode::Claude, "claude.denied.u5");
    }
    // Default No consumes the paste and exits without SessionStart or input.
    let declined = rig.launch(&config);
    rig.paste(&declined, "not a prompt");
    rig.wait_for_shell(&declined);
    assert_eq!(
        observe(StandInMode::Claude, &rig.capture_plain(&declined)),
        ScreenFacts::default()
    );
    assert!(!rig.journal(&declined).iter().any(|e| e["kind"] == "event"));

    let mut config = StandInConfig::new(StandInMode::Codex);
    config.manual_clock = true;
    config.update_prompt = true;
    config.folder_trust = true;
    config.hooks_review = true;
    config.worktree_choice = true;
    config.permission_dialog = true;
    let pane = rig.launch(&config);
    assert_source(&rig, &pane, StandInMode::Codex, "codex.update.u5");
    for (key, frame, source) in [
        ("Escape", "folder-trust", "codex.folder-trust.p1"),
        ("Enter", "hooks-review", "codex.hooks-review.p1"),
        ("Enter", "hooks-list", "codex.hooks-list.p1"),
        ("Escape", "hooks-review", "codex.hooks-review.p1"),
    ] {
        let after = rig.event_cursor(&pane);
        rig.key(&pane, key);
        rendered(&rig, &pane, after, frame);
        assert_source(&rig, &pane, StandInMode::Codex, source);
    }
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "Down");
    rig.key(&pane, "Enter");
    rendered(&rig, &pane, after, "fresh");
    assert_source(&rig, &pane, StandInMode::Codex, "codex.fresh.u5");
    // Worktree cancellation restores the prior editor, without an invented ID.
    let after = rig.event_cursor(&pane);
    rig.paste(&pane, "/new");
    rendered(&rig, &pane, after, "worktree");
    assert_source(&rig, &pane, StandInMode::Codex, "codex.worktree.p1");
    let after = rig.event_cursor(&pane);
    rig.key(&pane, "Escape");
    rendered(&rig, &pane, after, "fresh");
    assert_source(&rig, &pane, StandInMode::Codex, "codex.fresh.u5");
    let after = rig.event_cursor(&pane);
    rig.paste(&pane, "/new");
    rendered(&rig, &pane, after, "worktree");
    let after = rig.event_cursor(&pane);
    rig.paste(&pane, "modal input must not be delivered");
    rendered(&rig, &pane, after, "clearing");
    let after = rig.event_cursor(&pane);
    rig.control(
        &pane,
        serde_json::json!({"command":"advance_to", "now_ms":100}),
    );
    rendered(&rig, &pane, after, "clear-complete");
    assert_source(&rig, &pane, StandInMode::Codex, "codex.clear-complete.u5");
    for cancel in [false, true] {
        let after = rig.event_cursor(&pane);
        rig.paste(&pane, "/atm/dialog");
        rendered(&rig, &pane, after, "permission");
        assert_source(&rig, &pane, StandInMode::Codex, "codex.permission.p1");
        if !cancel {
            let after = rig.event_cursor(&pane);
            rig.key(&pane, "Down");
            rig.key(&pane, "Down"); // Both Yes options approve; option 3 denies.
            rig.wait_event(&pane, after, |e| {
                e["kind"] == "rendered" && e["selection"] == 3
            });
            assert_eq!(
                observe(StandInMode::Codex, &rig.capture_plain(&pane)),
                ScreenFacts::default()
            );
        }
        let after = rig.event_cursor(&pane);
        rig.key(&pane, if cancel { "Escape" } else { "Enter" });
        rendered(&rig, &pane, after, "interrupted");
        assert_source(&rig, &pane, StandInMode::Codex, "codex.interrupted.u5");
    }
    assert!(!rig
        .journal(&pane)
        .iter()
        .any(|e| e["event"] == "UserPromptSubmit"));
    rig.close();
}

#[test]
fn permission_yes_runs_before_completion_and_hooks_precede_the_modal() {
    let mut rig = StandInRig::start();
    for (mode, option) in [
        (StandInMode::Claude, 1),
        (StandInMode::Codex, 1),
        (StandInMode::Codex, 2),
    ] {
        let mut config = StandInConfig::new(mode);
        config.manual_clock = true;
        config.permission_dialog = true;
        let pane = rig.launch(&config);
        let after = rig.event_cursor(&pane);
        rig.paste(&pane, "/atm/dialog");
        rendered(&rig, &pane, after, "permission");
        let events = rig.journal(&pane);
        let permission = events
            .iter()
            .position(|e| e["event"] == "PermissionRequest")
            .unwrap();
        let modal = events
            .iter()
            .position(|e| e["kind"] == "render" && e["frame"] == "permission")
            .unwrap();
        assert!(permission < modal);
        rig.wait_for_row(&pane, |row| {
            row.status == atm_core::SessionStatus::AttentionNeeded
        });
        for _ in 1..option {
            rig.key(&pane, "Down");
        }
        let after = rig.event_cursor(&pane);
        rig.key(&pane, "Enter");
        rendered(&rig, &pane, after, "working");
        assert_eq!(
            observe(mode, &rig.capture_plain(&pane)).turn,
            TurnState::Running
        );
        assert!(!rig
            .journal(&pane)
            .iter()
            .any(|e| e["event"] == "PostToolUse"));
        let after = rig.event_cursor(&pane);
        rig.control(
            &pane,
            serde_json::json!({"command":"advance_to", "now_ms":500}),
        );
        rendered(&rig, &pane, after, "idle");
        let events = rig.journal(&pane);
        let post = events
            .iter()
            .position(|e| e["event"] == "PostToolUse")
            .unwrap();
        let stop = events.iter().position(|e| e["event"] == "Stop").unwrap();
        assert!(post < stop);
        assert_eq!(
            observe(mode, &rig.capture_plain(&pane)).turn,
            TurnState::NotRunning
        );
    }
    rig.close();
}

#[derive(Debug, PartialEq, Eq)]
struct Cell {
    ch: char,
    dim: bool,
    reverse: bool,
}

fn cells(text: &str) -> Vec<Cell> {
    // Test instrumentation, not an observer: decode the two attributes under
    // contract, skipping extended color arguments rather than treating RGB 2
    // as the dim attribute. tmux is allowed to normalize SGR encodings.
    let mut chars = text.chars();
    let (mut dim, mut reverse) = (false, false);
    let mut result = Vec::new();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            assert_eq!(chars.next(), Some('['));
            let mut params = String::new();
            for ch in chars.by_ref() {
                if ch == 'm' {
                    break;
                }
                params.push(ch);
            }
            let mut codes = params.split(';').map(|s| s.parse::<u16>().unwrap_or(0));
            while let Some(code) = codes.next() {
                match code {
                    0 => {
                        dim = false;
                        reverse = false;
                    }
                    2 => dim = true,
                    22 => dim = false,
                    7 => reverse = true,
                    27 => reverse = false,
                    38 | 48 | 58 => {
                        let count = match codes.next() {
                            Some(2) => 3,
                            Some(5) => 1,
                            other => panic!("unsupported color {other:?}"),
                        };
                        for _ in 0..count {
                            codes.next().unwrap();
                        }
                    }
                    _ => {}
                }
            }
        } else {
            result.push(Cell { ch, dim, reverse });
        }
    }
    result
}

#[test]
fn styled_capture_preserves_faint_placeholders_normal_drafts_and_cursor_spaces() {
    let mut rig = StandInRig::start();
    for mode in [StandInMode::Claude, StandInMode::Codex] {
        let config = StandInConfig::new(mode);
        let pane = rig.launch(&config);
        for source in sources()
            .iter()
            .filter(|s| s.mode == mode.name() && s.style.is_some())
        {
            replay(&rig, &pane, source);
            let captured = rig.capture_styled(&pane);
            let original = read(source.style.as_deref().unwrap());
            let want = cells(body(&original));
            let actual = cells(&captured);
            // Plain text/layout is covered separately; -N must also retain the
            // reverse-video ASCII cursor cell omitted from a plain capture.
            let significant = |items: Vec<Cell>| {
                items
                    .into_iter()
                    .filter(|c| c.ch != ' ' || c.reverse)
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                significant(actual),
                significant(want),
                "style/cursor cells: {}",
                source.key
            );
            if source.key.ends_with("fresh.rich-enabled") || source.key.ends_with("fresh.rich") {
                assert!(
                    cells(&captured)
                        .iter()
                        .any(|c| c.dim && c.ch.is_alphabetic()),
                    "faint placeholder: {}",
                    source.key
                );
            }
            if source.key.contains(".draft.") {
                let decoded = cells(&captured);
                let text: String = decoded.iter().map(|c| c.ch).collect();
                let prefix = text
                    .split_once("baseline draft 123")
                    .unwrap()
                    .0
                    .chars()
                    .count();
                assert!(decoded.iter().skip(prefix).take(18).all(|c| !c.dim));
                if mode == StandInMode::Claude {
                    assert!(decoded.iter().any(|c| c.reverse && c.ch == ' '));
                }
            }
        }
    }
    rig.close();
}

#[test]
fn every_native_source_replayed_through_tmux_matches_real_fixture_and_pinned_facts() {
    let mut rig = StandInRig::start();
    let sources = sources();
    assert_eq!(
        sources.len(),
        74,
        "review expectations when the catalog grows"
    );
    for mode in [StandInMode::Claude, StandInMode::Codex, StandInMode::Pi] {
        let mut config = StandInConfig::new(mode);
        config.manual_clock = true;
        let pane = rig.launch(&config);
        for source in sources.iter().filter(|s| s.mode == mode.name()) {
            let original = read(&source.source);
            let original = body(&original);
            let want = expected(&source.key);
            assert_eq!(
                observe(mode, original),
                want,
                "independent fixture expectation: {}",
                source.key
            );
            let captured = replay(&rig, &pane, source);
            assert_eq!(
                normalize(&captured),
                normalize(original),
                "native text/layout: {}",
                source.key
            );
            assert_eq!(
                observe(mode, &captured),
                want,
                "captured pane: {}",
                source.key
            );
            // A source-only replay must not masquerade as runtime readiness.
            let barrier = rig.control(&pane, serde_json::json!({"command":"barrier"}));
            assert!(!rig
                .journal(&pane)
                .iter()
                .any(|e| e["kind"] == "ready" && e["sequence"] == barrier["render_sequence"]));
        }
    }
    rig.close();
}
