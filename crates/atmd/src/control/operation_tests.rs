//! Characterize the legacy commands' resolution, effects, filtering and errors.

use std::error::Error;

use atm_core::{SessionId, SessionView};
use atm_tmux::{mock::MockCall, MockTmuxClient, TmuxClient, TmuxError};

use super::{interrupt, kill, peek, reply, ControlError, ReplyOutcome};

fn session(id: &str, short: &str, pane: Option<&str>) -> SessionView {
    SessionView {
        id: SessionId::new(id),
        id_short: short.into(),
        tmux_pane: pane.map(str::to_string),
        ..SessionView::default()
    }
}

fn keys(pane: &str, keys: &str) -> MockCall {
    MockCall::SendKeys {
        pane: pane.into(),
        keys: keys.into(),
    }
}

async fn run_operation(
    operation: &str,
    client: &dyn TmuxClient,
    sessions: &[SessionView],
    target: &str,
) -> Result<(), ControlError> {
    match operation {
        "kill" => kill(client, sessions, target).await.map(|_| ()),
        "interrupt" => interrupt(client, sessions, target).await.map(|_| ()),
        "reply" => reply(client, sessions, target, None, false, false)
            .await
            .map(|_| ()),
        "escape" => reply(client, sessions, target, None, false, true)
            .await
            .map(|_| ()),
        "numbered" => reply(client, sessions, target, Some(2), false, false)
            .await
            .map(|_| ()),
        "peek" => peek(client, sessions, target, None, false)
            .await
            .map(|_| ()),
        _ => panic!("unknown test operation"),
    }
}

// Bypassing resolution, deduplicating rows or touching tmux before refusal breaks this.
#[tokio::test]
async fn operations_resolve_before_tmux_and_keep_refusals() {
    for (sessions, target, expected) in [
        (vec![], "missing", "No session matching 'missing'"),
        (
            vec![session("full-id", "short", None)],
            "full",
            "Session short has no tmux pane",
        ),
        (
            vec![
                session("same-id", "second", Some("%8")),
                session("same-id", "first", Some("%7")),
            ],
            "same",
            "Ambiguous target 'same' matches 2 sessions: second, first",
        ),
    ] {
        for operation in ["kill", "interrupt", "reply", "peek"] {
            let client = MockTmuxClient::new();
            let error = run_operation(operation, &client, &sessions, target)
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), expected, "{operation}");
            assert!(client.calls().is_empty(), "{operation}");
        }
    }
    let sessions = [session("full-id", "short", Some("%7"))];
    for operation in ["kill", "interrupt", "reply", "peek"] {
        for (target, pane) in [("full", "%7"), ("sho", "%7"), ("%anything", "%anything")] {
            let client = MockTmuxClient::new();
            run_operation(operation, &client, &sessions, target)
                .await
                .unwrap();
            let expected = match operation {
                "kill" => MockCall::KillPane { pane: pane.into() },
                "interrupt" => keys(pane, "C-c"),
                "reply" => keys(pane, "Enter"),
                "peek" => MockCall::CapturePane { pane: pane.into() },
                _ => unreachable!(),
            };
            assert_eq!(client.calls(), vec![expected], "{operation}: {target}");
        }
    }
}

// Wrong keys, wrong targets, extra checks or a lost return value break this.
#[tokio::test]
async fn kill_and_interrupt_return_the_pane_and_exact_effect() {
    let client = MockTmuxClient::new();
    let sessions = [session("full-id", "short", Some("%7"))];
    assert_eq!(kill(&client, &sessions, "full").await.unwrap(), "%7");
    assert_eq!(
        client.calls(),
        vec![MockCall::KillPane { pane: "%7".into() }]
    );
    client.clear_calls();
    assert_eq!(interrupt(&client, &sessions, "sho").await.unwrap(), "%7");
    assert_eq!(client.calls(), vec![keys("%7", "C-c")]);
}

// Changing no-first/yes-before-option precedence or capturing shortcuts breaks this.
#[tokio::test]
async fn reply_shortcuts_do_not_capture() {
    for (option, yes, no, expected, key) in [
        (
            None,
            false,
            false,
            ReplyOutcome::Enter { pane: "%7".into() },
            "Enter",
        ),
        (
            None,
            true,
            false,
            ReplyOutcome::Enter { pane: "%7".into() },
            "Enter",
        ),
        (
            Some(3),
            true,
            false,
            ReplyOutcome::Enter { pane: "%7".into() },
            "Enter",
        ),
        (
            None,
            false,
            true,
            ReplyOutcome::Escape { pane: "%7".into() },
            "Escape",
        ),
        (
            Some(3),
            true,
            true,
            ReplyOutcome::Escape { pane: "%7".into() },
            "Escape",
        ),
    ] {
        let client = MockTmuxClient::new();
        assert_eq!(
            reply(&client, &[], "%7", option, yes, no).await.unwrap(),
            expected
        );
        assert_eq!(client.calls(), vec![keys("%7", key)]);
    }
}

// Swapping Down/Up, changing the fallback, omitting Enter or rejecting zero breaks this.
#[tokio::test]
async fn reply_numbered_navigation_preserves_direction_and_default() {
    for (content, desired, expected_keys) in [
        (vec!["  ❯ 2. café"], 4, vec!["Down", "Down", "Enter"]),
        (vec!["❯ 2. café"], 1, vec!["Up", "Enter"]),
        (vec!["❯ 2. café"], 2, vec!["Enter"]),
        (vec!["no cursor"], 2, vec!["Down", "Enter"]),
        (vec!["❯ not a number. text"], 0, vec!["Up", "Enter"]),
        (
            vec!["❯ 12. text", "❯ 1. later cursor"],
            10,
            vec!["Up", "Up", "Enter"],
        ),
    ] {
        let client = MockTmuxClient::new();
        client.set_pane_content("%7", content.into_iter().map(String::from).collect());
        assert_eq!(
            reply(&client, &[], "%7", Some(desired), false, false)
                .await
                .unwrap(),
            ReplyOutcome::Selected {
                pane: "%7".into(),
                option: desired
            }
        );
        let mut expected = vec![MockCall::CapturePane { pane: "%7".into() }];
        expected.extend(expected_keys.into_iter().map(|key| keys("%7", key)));
        assert_eq!(client.calls(), expected);
    }
}

// Reversing prompt/tail precedence or changing capture boundaries/lines breaks this.
#[tokio::test]
async fn peek_preserves_prompt_tail_and_all_lines() {
    let lines: Vec<String> = [
        "earlier output",
        "",
        "Choose café?",
        "❯ 1. Sí",
        "  2. No",
        "Enter to select",
        "after footer",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let prompt: Vec<String> = ["Choose café?", "❯ 1. Sí", "  2. No", "Enter to select"]
        .into_iter()
        .map(String::from)
        .collect();
    for (tail, use_prompt, expected) in [
        (None, false, lines.clone()),
        (Some(0), false, vec![]),
        (Some(1), false, vec!["after footer".into()]),
        (Some(100), false, lines.clone()),
        (None, true, prompt.clone()),
        (Some(0), true, prompt),
    ] {
        let client = MockTmuxClient::new();
        client.set_pane_content("%7", lines.clone());
        assert_eq!(
            peek(&client, &[], "%7", tail, use_prompt).await.unwrap(),
            expected
        );
        assert_eq!(
            client.calls(),
            vec![MockCall::CapturePane { pane: "%7".into() }]
        );
    }
    for (tail, prompt) in [(None, false), (Some(0), false), (Some(10), true)] {
        assert!(peek(&MockTmuxClient::new(), &[], "%7", tail, prompt)
            .await
            .unwrap()
            .is_empty());
    }
}

// Dropping the source, changing context or continuing after an error breaks this.
#[tokio::test]
async fn operation_errors_keep_context_and_source() {
    for (operation, context, expected_call) in [
        (
            "kill",
            "Failed to kill pane %7",
            MockCall::KillPane { pane: "%7".into() },
        ),
        (
            "interrupt",
            "Failed to interrupt pane %7",
            keys("%7", "C-c"),
        ),
        ("reply", "Failed to send Enter", keys("%7", "Enter")),
        ("escape", "Failed to send Escape", keys("%7", "Escape")),
        (
            "numbered",
            "Failed to capture pane",
            MockCall::CapturePane { pane: "%7".into() },
        ),
        (
            "peek",
            "Failed to capture pane %7",
            MockCall::CapturePane { pane: "%7".into() },
        ),
    ] {
        let client = MockTmuxClient::new();
        client.set_next_error(TmuxError::NotFound);
        let error = run_operation(operation, &client, &[], "%7")
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), context);
        assert_eq!(
            error.source().unwrap().to_string(),
            "tmux not found in PATH"
        );
        assert_eq!(client.calls(), vec![expected_call]);
    }
}

// An accidental Sized requirement on any extracted function fails to compile here.
#[tokio::test]
async fn operations_accept_trait_objects() {
    let mock = MockTmuxClient::new();
    let client: &dyn TmuxClient = &mock;
    assert_eq!(kill(client, &[], "%7").await.unwrap(), "%7");
    assert_eq!(interrupt(client, &[], "%7").await.unwrap(), "%7");
    assert_eq!(
        reply(client, &[], "%7", None, false, false).await.unwrap(),
        ReplyOutcome::Enter { pane: "%7".into() }
    );
    assert_eq!(
        peek(client, &[], "%7", None, false).await.unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        mock.calls(),
        vec![
            MockCall::KillPane { pane: "%7".into() },
            keys("%7", "C-c"),
            keys("%7", "Enter"),
            MockCall::CapturePane { pane: "%7".into() }
        ]
    );
}
