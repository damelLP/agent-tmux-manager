//! The preparation boundary must not type; every key, including the final
//! Enter, must consult admission. External effects are the mock tmux boundary.
use super::*;
use crate::control::service::ServiceError;
use atm_tmux::{mock::MockCall, MockTmuxClient};

fn key(name: &str) -> MockCall {
    MockCall::SendKeys {
        pane: "%7".into(),
        keys: name.into(),
    }
}

#[tokio::test]
async fn preparation_captures_without_typing_and_finish_keeps_navigation() {
    for (option, yes, no, content, expected, expected_keys) in [
        (
            None,
            false,
            false,
            vec![],
            ReplyOutcome::Enter { pane: "%7".into() },
            vec!["Enter"],
        ),
        (
            Some(3),
            true,
            true,
            vec![],
            ReplyOutcome::Escape { pane: "%7".into() },
            vec!["Escape"],
        ),
        (
            Some(4),
            false,
            false,
            vec!["  ❯ 2. café"],
            ReplyOutcome::Selected {
                pane: "%7".into(),
                option: 4,
            },
            vec!["Down", "Down", "Enter"],
        ),
        (
            Some(0),
            false,
            false,
            vec!["no cursor"],
            ReplyOutcome::Selected {
                pane: "%7".into(),
                option: 0,
            },
            vec!["Up", "Enter"],
        ),
        (
            Some(10),
            false,
            false,
            vec!["❯ 12. first", "❯ 1. later"],
            ReplyOutcome::Selected {
                pane: "%7".into(),
                option: 10,
            },
            vec!["Up", "Up", "Enter"],
        ),
    ] {
        let mock = MockTmuxClient::new();
        mock.set_pane_content("%7", content.into_iter().map(String::from).collect());
        let prepared = prepare_reply(&mock, "%7".into(), option, yes, no)
            .await
            .unwrap();
        let mut calls = if !yes && !no && option.is_some() {
            vec![MockCall::CapturePane { pane: "%7".into() }]
        } else {
            vec![]
        };
        assert_eq!(mock.calls(), calls, "preparing must never press a key");
        let mut admitted = 0;
        let outcome: Result<_, ControlError> = finish_reply(&mock, prepared, || {
            admitted += 1;
            Ok(())
        })
        .await;
        assert_eq!(outcome.unwrap(), expected);
        assert_eq!(admitted, expected_keys.len());
        calls.extend(expected_keys.into_iter().map(key));
        assert_eq!(mock.calls(), calls);
    }
}

#[tokio::test]
async fn every_key_including_enter_requires_admission() {
    for (option, allow, expected_keys) in [
        (Some(3), 2, vec!["Down", "Down"]),
        (Some(usize::MAX), 1, vec!["Down"]),
        (None, 0, vec![]),
    ] {
        let mock = MockTmuxClient::new();
        let prepared = prepare_reply(&mock, "%7".into(), option, false, false)
            .await
            .unwrap();
        mock.clear_calls();
        let mut remaining = allow;
        let error: Result<_, ServiceError> = finish_reply(&mock, prepared, || {
            if remaining == 0 {
                return Err(ServiceError::Timeout);
            }
            remaining -= 1;
            Ok(())
        })
        .await;
        assert!(matches!(error, Err(ServiceError::Timeout)));
        assert_eq!(
            mock.calls(),
            expected_keys.into_iter().map(key).collect::<Vec<_>>()
        );
    }
}
