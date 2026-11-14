//! Characterize control boundaries: resolution, tmux ordering, cwd and errors.

use std::error::Error;

use atm_core::{SessionId, SessionStatus, SessionView};
use atm_tmux::{mock::MockCall, MockTmuxClient, PaneDirection, PaneInfo, TmuxError};

use super::{
    build_spawn_command, prepare_spawn, resolve_pane_id, send, split_spawn, type_spawn_command,
};

fn session(id: &str, short: &str, pane: Option<&str>) -> SessionView {
    SessionView {
        id: SessionId::new(id),
        id_short: short.into(),
        tmux_pane: pane.map(str::to_string),
        ..SessionView::default()
    }
}

fn pane(id: &str, active: bool) -> PaneInfo {
    PaneInfo {
        pane_id: id.into(),
        session_name: "probe".into(),
        window_index: 0,
        pane_pid: 1,
        width: 80,
        height: 24,
        is_active: active,
    }
}

#[test]
fn target_resolution_preserves_literal_panes_and_prefix_ambiguity() {
    assert_eq!(resolve_pane_id(&[], "%anything").unwrap(), "%anything");
    assert_eq!(
        resolve_pane_id(&[], "missing").unwrap_err().to_string(),
        "No session matching 'missing'"
    );
    let sessions = [session("full-id", "short", Some("%7"))];
    for target in ["full", "sho"] {
        assert_eq!(resolve_pane_id(&sessions, target).unwrap(), "%7");
    }
    assert_eq!(
        resolve_pane_id(&[session("full-id", "short", None)], "full")
            .unwrap_err()
            .to_string(),
        "Session short has no tmux pane"
    );
    // Duplicate rows are not deduplicated; messages retain input order.
    let sessions = [
        session("same-id", "second", Some("%8")),
        session("same-id", "first", Some("%7")),
    ];
    assert_eq!(
        resolve_pane_id(&sessions, "same").unwrap_err().to_string(),
        "Ambiguous target 'same' matches 2 sessions: second, first"
    );
}

#[tokio::test]
async fn send_reads_state_then_pastes_and_submits_separately() {
    for text in ["", "run tests;", "a\nb"] {
        let client = MockTmuxClient::new();
        client.set_next_raw_output("0 1\n");
        let sessions = [session("lead", "lead", Some("%7"))];
        assert_eq!(send(&client, &sessions, "lead", text).await.unwrap(), "%7");
        assert_eq!(
            client.calls(),
            vec![
                MockCall::RawCommand {
                    subcommand: "display-message".into(),
                    args: vec![
                        "-p".into(),
                        "-t".into(),
                        "%7".into(),
                        "#{pane_in_mode} #{bracket_paste_flag}".into()
                    ],
                },
                MockCall::PasteText {
                    pane: "%7".into(),
                    text: text.into()
                },
                MockCall::SendKeys {
                    pane: "%7".into(),
                    keys: "Enter".into()
                },
            ]
        );
    }
}

#[tokio::test]
async fn send_accepts_trait_objects() {
    let mock = MockTmuxClient::new();
    mock.set_next_raw_output("0 1\n");
    let client: &dyn atm_tmux::TmuxClient = &mock;

    assert_eq!(send(client, &[], "%7", "hello").await.unwrap(), "%7");
    assert_eq!(mock.call_count(), 3);
}

#[tokio::test]
async fn spawn_accepts_trait_objects() {
    let mock = MockTmuxClient::new();
    mock.set_next_pane_id("%42");
    let client: &dyn atm_tmux::TmuxClient = &mock;

    let context = prepare_spawn(client, Some("/work"), Some("%7"), None, None)
        .await
        .unwrap();
    let new_pane = split_spawn(client, &context, PaneDirection::Below, "30%")
        .await
        .unwrap();
    assert_eq!(new_pane, "%42");
    type_spawn_command(client, &new_pane, "agent")
        .await
        .unwrap();
    assert_eq!(mock.call_count(), 4);
}

#[tokio::test]
async fn resolution_and_state_failures_never_paste() {
    let client = MockTmuxClient::new();
    assert!(send(&client, &[], "absent", "hi").await.is_err());
    assert!(client.calls().is_empty());
    client.set_next_error(TmuxError::NotFound);
    let error = send(&client, &[], "%7", "hi").await.unwrap_err();
    assert_eq!(error.to_string(), "Failed to read the state of pane %7");
    assert_eq!(
        error.source().unwrap().to_string(),
        "tmux not found in PATH"
    );
    assert_eq!(client.call_count(), 1);
}

#[tokio::test]
async fn refusal_never_pastes_and_preserves_reason_precedence() {
    let client = MockTmuxClient::new();
    client.set_next_raw_output("1 0\n");
    let sessions = [SessionView {
        status: SessionStatus::AttentionNeeded,
        ..session("lead", "lead", Some("%7"))
    }];
    let error = send(&client, &sessions, "%7", "a\nb").await.unwrap_err();
    assert_eq!(error.to_string(), "Refusing to send to %7: the session is waiting on a dialog that Enter would answer; use `atm reply`, or, if the dialog is gone, type in the pane");
    assert_eq!(client.call_count(), 1);
}

#[tokio::test]
async fn spawn_target_and_cwd_follow_explicit_caller_active_precedence() {
    for (target, caller, expected) in [
        (Some("%9"), Some("%7"), "%9"),
        (None, Some("%7"), "%7"),
        (None, None, "%1"),
    ] {
        let client = MockTmuxClient::new();
        client.set_panes(vec![pane("%1", true), pane("%7", false), pane("%9", false)]);
        client.set_pane_cwd(expected, "/pane");
        let context = prepare_spawn(&client, None, target, caller, Some("/fallback"))
            .await
            .unwrap();
        assert_eq!(context.target_pane, expected);
        assert_eq!(context.cwd.as_deref(), Some("/pane"));
        assert_eq!(
            client.calls(),
            vec![
                MockCall::ListPanes,
                MockCall::GetPaneCwd {
                    pane: expected.into()
                }
            ]
        );
    }
}

#[tokio::test]
async fn spawn_cwd_is_plain_input_and_empty_explicit_cwd_bypasses_query() {
    for cwd in [Some(""), Some("/explicit")] {
        let client = MockTmuxClient::new();
        let context = prepare_spawn(&client, cwd, None, None, Some("/fallback"))
            .await
            .unwrap();
        assert_eq!(context.target_pane, "%0");
        assert_eq!(context.cwd.as_deref(), cwd);
        assert_eq!(client.calls(), vec![MockCall::ListPanes]);
    }
    for fallback in [None, Some("/client/not/daemon")] {
        let client = MockTmuxClient::new();
        let context = prepare_spawn(&client, None, None, None, fallback)
            .await
            .unwrap();
        assert_eq!(context.target_pane, "%0");
        assert_eq!(context.cwd.as_deref(), fallback);
        assert_eq!(
            client.calls(),
            vec![
                MockCall::ListPanes,
                MockCall::GetPaneCwd { pane: "%0".into() }
            ]
        );
    }
}

#[tokio::test]
async fn spawn_stops_on_list_failure_even_with_an_explicit_target() {
    let client = MockTmuxClient::new();
    client.set_next_error(TmuxError::NotFound);
    let error = prepare_spawn(&client, Some("/work"), Some("%7"), None, None)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Failed to list tmux panes");
    assert_eq!(
        error.source().unwrap().to_string(),
        "tmux not found in PATH"
    );
    assert_eq!(client.calls(), vec![MockCall::ListPanes]);
}

#[tokio::test]
async fn spawn_split_hint_only_identifies_a_missing_implicit_caller() {
    for (target, caller, expected) in [
        (
            None,
            Some("%99"),
            "Failed to split $TMUX_PANE (%99); pass -t <pane> to split another pane",
        ),
        (Some("%99"), Some("%99"), "Failed to split tmux pane"),
        (None, Some("%7"), "Failed to split tmux pane"),
        (None, None, "Failed to split tmux pane"),
    ] {
        let client = MockTmuxClient::new();
        client.set_panes(vec![pane("%7", true)]);
        let context = prepare_spawn(&client, Some("/work"), target, caller, None)
            .await
            .unwrap();
        client.clear_calls();
        client.set_next_error(TmuxError::CommandFailed {
            command: "split-window".into(),
            stderr: "probe".into(),
        });
        let error = split_spawn(&client, &context, PaneDirection::Below, "30%")
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), expected);
        assert_eq!(
            error.source().unwrap().to_string(),
            "tmux command failed: split-window — probe"
        );
        assert_eq!(client.call_count(), 1);
    }
}

#[tokio::test]
async fn split_returns_the_pane_before_typing_then_command_and_enter_are_separate() {
    let client = MockTmuxClient::new();
    let context = prepare_spawn(&client, None, Some("%7"), None, None)
        .await
        .unwrap();
    client.clear_calls();
    client.set_next_pane_id("%42");
    let new_pane = split_spawn(&client, &context, PaneDirection::Left, "20%")
        .await
        .unwrap();
    assert_eq!(new_pane, "%42");
    assert_eq!(
        client.calls(),
        vec![MockCall::SplitWindow {
            target: "%7".into(),
            size: "20%".into(),
            direction: PaneDirection::Left,
            command: None,
            cwd: None,
        }]
    );
    client.clear_calls();
    type_spawn_command(&client, &new_pane, "agent;")
        .await
        .unwrap();
    assert_eq!(
        client.calls(),
        vec![
            MockCall::SendKeys {
                pane: "%42".into(),
                keys: "agent;".into()
            },
            MockCall::SendKeys {
                pane: "%42".into(),
                keys: "Enter".into()
            },
        ]
    );
}

#[tokio::test]
async fn typing_failure_does_not_submit_the_command() {
    let client = MockTmuxClient::new();
    client.set_next_error(TmuxError::NotFound);
    let error = type_spawn_command(&client, "%42", "agent")
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Failed to send command to new pane");
    assert_eq!(
        error.source().unwrap().to_string(),
        "tmux not found in PATH"
    );
    assert_eq!(
        client.calls(),
        vec![MockCall::SendKeys {
            pane: "%42".into(),
            keys: "agent".into()
        }]
    );
}

#[test]
fn spawn_override_prefers_nonempty_specific_value_without_trimming() {
    for (specific, global, expected) in [
        (None, None, None),
        (Some("specific"), Some("global"), Some("specific")),
        (None, Some("global"), Some("global")),
        (Some(""), Some("global"), Some("global")),
        (Some(""), Some(""), None),
        (None, Some(""), None),
        (Some("specific"), None, Some("specific")),
        (Some("   "), Some("global"), Some("   ")),
        (None, Some("  global  "), Some("  global  ")),
    ] {
        let env = std::collections::HashMap::from([
            ("ATM_SPAWN_CLAUDE_BIN", specific),
            ("ATM_SPAWN_BIN", global),
        ]);
        assert_eq!(
            super::spawn_env_override("claude", "BIN", |key| {
                env.get(key).copied().flatten().map(str::to_string)
            })
            .as_deref(),
            expected,
            "specific={specific:?}, global={global:?}"
        );
    }
}

#[test]
fn spawn_override_normalizes_harness_ids_and_selects_each_suffix_independently() {
    let env = std::collections::HashMap::from([
        ("ATM_SPAWN_MY_AGENT_BIN", "custom"),
        ("ATM_SPAWN_BIN", "global"),
        ("ATM_SPAWN_ARGS", "global args"),
        ("ATM_SPAWN_PI_ARGS", "  x pi  "),
    ]);
    for (harness, suffix, expected) in [
        ("my-agent", "BIN", "custom"),
        ("my-agent", "ARGS", "global args"),
        ("pi", "BIN", "global"),
        ("pi", "ARGS", "  x pi  "),
    ] {
        assert_eq!(
            super::spawn_env_override(harness, suffix, |key| {
                env.get(key).map(|value| (*value).to_string())
            })
            .as_deref(),
            Some(expected),
            "{harness} {suffix}"
        );
    }
}

#[test]
fn spawn_override_whitespace_args_mask_global_args() {
    let env =
        std::collections::HashMap::from([("ATM_SPAWN_PI_ARGS", "   "), ("ATM_SPAWN_ARGS", "x pi")]);
    assert_eq!(
        super::spawn_env_override("pi", "ARGS", |key| {
            env.get(key).map(|value| (*value).to_string())
        })
        .as_deref(),
        Some("   ")
    );
}

// These cases catch required/role arguments being lost under BIN, or placed
// before a wrapper's raw prefix or after its model argument.
#[test]
fn pure_builder_keeps_required_and_role_args_under_binary_overrides() {
    let defaults = ["x".into(), "codex".into()];
    let role = ["--profile".into(), "a b".into()];
    for (binary, defaults, bin, raw, extra, flag, model, expected) in [
        (
            "codex",
            &[][..],
            None,
            None,
            &[][..],
            None,
            None,
            "codex '--no-daemon'",
        ),
        (
            "mise",
            &defaults[..],
            Some("/my stand-in"),
            None,
            &[][..],
            None,
            None,
            "'/my stand-in' '--no-daemon'",
        ),
        (
            "codex",
            &[][..],
            Some("mise"),
            Some("x codex"),
            &[][..],
            None,
            None,
            "mise x codex '--no-daemon'",
        ),
        (
            "mise",
            &defaults[..],
            None,
            None,
            &[][..],
            None,
            None,
            "mise 'x' 'codex' '--no-daemon'",
        ),
        (
            "mise",
            &defaults[..],
            Some("/my stand-in"),
            None,
            &role[..],
            Some("-m"),
            Some("role model"),
            "'/my stand-in' '--no-daemon' '--profile' 'a b' -m 'role model'",
        ),
    ] {
        assert_eq!(
            build_spawn_command(
                binary,
                defaults,
                flag,
                None,
                model,
                bin,
                raw,
                extra,
                &["--no-daemon"]
            ),
            expected
        );
    }
}

#[test]
fn launch_policy_requirements_reach_the_shared_builder() {
    use atm_core::Harness;
    for (harness, binary, expected) in [
        (Harness::ClaudeCode, "claude", "claude"),
        (Harness::Pi, "pi", "pi"),
        (Harness::Codex, "codex", "codex '--no-daemon'"),
    ] {
        let policy = super::launch_policy(harness).unwrap();
        assert_eq!(
            build_spawn_command(
                binary,
                &[],
                None,
                None,
                None,
                None,
                None,
                &[],
                policy.required_args
            ),
            expected
        );
    }
    for harness in [
        Harness::Amp,
        Harness::Qwen,
        Harness::Gemini,
        Harness::Unknown,
    ] {
        assert!(super::launch_policy(harness).is_none());
    }
}

#[test]
fn empty_requirements_preserve_legacy_command_bytes() {
    for binary in ["claude", "pi", "my-agent"] {
        for (bin, raw, expected_suffix) in [
            (None, None, " '--profile' 'a b' --model 'model name'"),
            (
                None,
                Some("   "),
                " '--profile' 'a b'  --model 'model name'",
            ),
            (
                Some("wrapper"),
                Some("  x --raw;  "),
                " x --raw; --model 'model name'",
            ),
        ] {
            let expected = format!(
                "cd '/work dir' && {}{expected_suffix}",
                bin.unwrap_or(binary)
            );
            assert_eq!(
                build_spawn_command(
                    binary,
                    &["--profile".into(), "a b".into()],
                    Some("--model"),
                    Some("/work dir"),
                    Some("model name"),
                    bin,
                    raw,
                    &[],
                    &[]
                ),
                expected
            );
        }
    }
}

#[test]
fn pure_builder_preserves_defaults_overrides_quoting_and_raw_arguments() {
    let defaults = ["--profile".into(), "a b".into()];
    assert_eq!(
        build_spawn_command(
            "agent",
            &defaults,
            Some("--model"),
            Some("/work/it's"),
            Some("model name"),
            None,
            None,
            &[],
            &[],
        ),
        "cd '/work/it'\\''s' && agent '--profile' 'a b' --model 'model name'"
    );
    assert_eq!(
        build_spawn_command(
            "agent",
            &defaults,
            None,
            None,
            Some("ignored"),
            Some("/my wrapper"),
            Some("  x --raw;  "),
            &[],
            &[],
        ),
        "'/my wrapper' x --raw;"
    );
    assert_eq!(
        build_spawn_command("agent", &[], None, None, None, None, Some("   "), &[], &[]),
        "agent "
    );
    assert_eq!(
        build_spawn_command(
            "agent",
            &defaults,
            None,
            None,
            None,
            Some("wrapper"),
            None,
            &[],
            &[]
        ),
        "wrapper"
    );
}
