use super::message_delivery_gate::{Effect, GatedTmux};
use crate::control::reservation::testing::ScriptedTmuxClient;
use atm_tmux::{mock::MockCall, TmuxClient, TmuxError};
use std::{future::Future, pin::Pin, task::Poll, time::Duration};

async fn guarded(test: impl Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(30), test)
        .await
        .expect("gate test hung");
}

async fn poll_once<F: Future + ?Sized>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}

async fn apply(client: &dyn TmuxClient, effect: Effect) -> Result<(), TmuxError> {
    match effect {
        Effect::PasteText => client.paste_text("%7", "draft").await,
        Effect::SendKeys => client.send_keys("%7", "Enter").await,
    }
}

fn recorded(effect: Effect) -> MockCall {
    match effect {
        Effect::PasteText => MockCall::PasteText {
            pane: "%7".into(),
            text: "draft".into(),
        },
        Effect::SendKeys => MockCall::SendKeys {
            pane: "%7".into(),
            keys: "Enter".into(),
        },
    }
}

// Catches holding before recording, lost entry notifications, and early return.
async fn held_effect(effect: Effect) {
    guarded(async {
        let inner = ScriptedTmuxClient::new();
        let gate = GatedTmux::new(inner.clone());
        gate.hold_next(effect).unwrap();
        let client: Box<dyn TmuxClient> = Box::new(gate.clone());
        let mut call = Box::pin(apply(client.as_ref(), effect));
        assert!(poll_once(call.as_mut()).await.is_pending());
        gate.entered().await; // Also works when observation starts after entry.
        assert_eq!(inner.mock().calls(), vec![recorded(effect)]);
        assert!(poll_once(call.as_mut()).await.is_pending());
        gate.release(); // Release before the next poll must not be lost.
        call.await.unwrap();
        assert_eq!(inner.mock().calls(), vec![recorded(effect)]);
    })
    .await;
}

#[tokio::test]
async fn held_paste_records_before_entry_and_returns_only_after_release() {
    held_effect(Effect::PasteText).await;
}

#[tokio::test]
async fn held_send_keys_records_before_entry_and_returns_only_after_release() {
    held_effect(Effect::SendKeys).await;
}

// Catches a pre-effect failure or an accidentally successful injected response.
async fn failed_effect(effect: Effect) {
    guarded(async {
        let inner = ScriptedTmuxClient::new();
        let gate = GatedTmux::new(inner.clone());
        gate.fail_next_after_effect(effect).unwrap();
        let error = apply(&gate, effect).await.unwrap_err();
        assert!(matches!(error, TmuxError::Timeout { .. }));
        assert_eq!(inner.mock().calls(), vec![recorded(effect)]);
        apply(&gate, effect).await.unwrap();
        assert_eq!(inner.mock().calls(), vec![recorded(effect); 2]);
    })
    .await;
}

#[tokio::test]
async fn failed_after_effect_paste_is_recorded() {
    failed_effect(Effect::PasteText).await;
}

#[tokio::test]
async fn failed_after_effect_send_keys_is_recorded() {
    failed_effect(Effect::SendKeys).await;
}

// Catches gating reads, holding the wrong effect, or gating subsequent calls.
#[tokio::test]
async fn only_next_matching_effect_is_held_and_reads_continue() {
    guarded(async {
        for effect in [Effect::PasteText, Effect::SendKeys] {
            let other = match effect {
                Effect::PasteText => Effect::SendKeys,
                Effect::SendKeys => Effect::PasteText,
            };
            let inner = ScriptedTmuxClient::new();
            inner.mock().set_pane_content("%7", vec!["visible".into()]);
            let gate = GatedTmux::new(inner.clone());
            gate.hold_next(effect).unwrap();
            assert!(gate.list_panes().await.unwrap().is_empty());
            apply(&gate, other).await.unwrap();
            let mut first = Box::pin(apply(&gate, effect));
            assert!(poll_once(first.as_mut()).await.is_pending());
            gate.entered().await;
            assert_eq!(gate.capture_pane("%7").await.unwrap(), vec!["visible"]);
            assert!(gate.list_panes().await.unwrap().is_empty());
            assert_eq!(gate.get_pane_cwd("%7").await.unwrap(), None);
            // Raw reads still use the scripted client's validation, not a gate.
            assert!(matches!(
                gate.raw_command("unsupported", &[]).await,
                Err(TmuxError::ParseError(_))
            ));
            apply(&gate, effect).await.unwrap();
            assert!(poll_once(first.as_mut()).await.is_pending());
            gate.release();
            first.await.unwrap();
            apply(&gate, effect).await.unwrap();
            assert_eq!(
                inner
                    .mock()
                    .calls()
                    .iter()
                    .filter(|c| **c == recorded(effect))
                    .count(),
                3
            );
        }
    })
    .await;
}

#[tokio::test]
async fn failure_is_not_consumed_by_reads_or_other_effect() {
    guarded(async {
        for effect in [Effect::PasteText, Effect::SendKeys] {
            let other = match effect {
                Effect::PasteText => Effect::SendKeys,
                Effect::SendKeys => Effect::PasteText,
            };
            let gate = GatedTmux::new(ScriptedTmuxClient::new());
            gate.fail_next_after_effect(effect).unwrap();
            gate.capture_pane("%7").await.unwrap();
            gate.list_panes().await.unwrap();
            apply(&gate, other).await.unwrap();
            assert!(matches!(
                apply(&gate, effect).await,
                Err(TmuxError::Timeout { .. })
            ));
            apply(&gate, effect).await.unwrap();
        }
    })
    .await;
}

// Catches replacement of an armed/held barrier and stale signals on reuse.
#[tokio::test]
async fn refuses_rearming_until_release_and_can_then_hold_again() {
    guarded(async {
        let gate = GatedTmux::new(ScriptedTmuxClient::new());
        gate.hold_next(Effect::PasteText).unwrap();
        assert!(gate.hold_next(Effect::SendKeys).is_err());
        assert!(gate.fail_next_after_effect(Effect::SendKeys).is_err());
        let mut entered = Box::pin(gate.entered());
        assert!(poll_once(entered.as_mut()).await.is_pending());
        let mut first = Box::pin(apply(&gate, Effect::PasteText));
        assert!(poll_once(first.as_mut()).await.is_pending());
        entered.await;
        assert!(gate.hold_next(Effect::SendKeys).is_err());
        assert!(gate.fail_next_after_effect(Effect::SendKeys).is_err());
        gate.release();
        first.await.unwrap();
        gate.hold_next(Effect::SendKeys).unwrap();
        let mut entered = Box::pin(gate.entered());
        assert!(poll_once(entered.as_mut()).await.is_pending());
        let mut second = Box::pin(apply(&gate, Effect::SendKeys));
        assert!(poll_once(second.as_mut()).await.is_pending());
        entered.await;
        gate.release();
        second.await.unwrap();
        gate.fail_next_after_effect(Effect::PasteText).unwrap();
        assert!(gate.hold_next(Effect::SendKeys).is_err());
        assert!(apply(&gate, Effect::PasteText).await.is_err());
        gate.hold_next(Effect::PasteText).unwrap();
    })
    .await;
}
