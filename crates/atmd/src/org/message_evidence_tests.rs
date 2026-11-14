//! Synthetic neutral events: test correlation, not a vendor payload schema.
use super::*;
use crate::control::reservation::PaneId;
use crate::org::messages::{MessageClock, MessageError};
use atm_core::{Harness, LifecycleEvent, SessionId};
use atm_org::{DeliveryReceipt, MessageId};
use chrono::DateTime;
use std::collections::HashSet;
use tokio::time::Instant;

fn evidence() -> MessageEvidence {
    MessageEvidence::new(MessageClock::new(
        Instant::now(),
        DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
    ))
}
fn id() -> MessageId {
    "m123".parse().unwrap()
}
fn scope() -> AttemptScope {
    AttemptScope {
        message_id: id(),
        attempt_seq: 124,
        recipient: "demo/worker".parse().unwrap(),
        pane: PaneId::parse("%7").unwrap(),
        harness: Harness::Pi,
        session_id: Some(SessionId::new("session")),
        excluded_sessions: HashSet::new(),
    }
}
const REFERENCE: &str = "[m123] Read: atm inbox --id m123";
fn armed() -> MessageEvidence {
    let evidence = evidence();
    evidence.register(id(), REFERENCE.into()).unwrap();
    evidence.arm(scope()).unwrap();
    evidence
}
fn observe(e: &MessageEvidence, session: &str, pane: &str, harness: Harness, text: &str) {
    e.observe(
        &SessionId::new(session),
        Some(pane),
        harness,
        &LifecycleEvent::PromptSubmit {
            prompt: Some(text.into()),
        },
    )
    .unwrap();
}
fn exact(e: &MessageEvidence) {
    observe(e, "session", "%7", Harness::Pi, REFERENCE);
}

#[test]
fn full_later_prompt_not_first_prompt_preview() {
    let e = armed();
    observe(
        &e,
        "session",
        "%7",
        Harness::Pi,
        "an earlier prompt whose preview is irrelevant",
    );
    assert!(e.receipt(id()).unwrap().is_none());
    exact(&e);
    let receipt = e.receipt(id()).unwrap().unwrap();
    assert_eq!(receipt.session_id.as_str(), "session");
    assert_eq!(receipt.confirmation, "prompt_submit_exact_reference");
}

#[test]
fn token_substring_and_body_mismatch_never_confirm() {
    for text in [
        "[m123]",
        "body",
        "[m123] Read: atm inbox --id m12",
        " [m123] Read: atm inbox --id m123",
        "[m123] Read: atm inbox --id m123\n",
    ] {
        let e = armed();
        observe(&e, "session", "%7", Harness::Pi, text);
        assert!(e.receipt(id()).unwrap().is_none(), "{text:?}");
        assert!(e.snapshot(id()).unwrap().probably_landed);
    }
}

#[test]
fn wrong_pane_session_harness_and_pending_never_confirm() {
    for (session, pane, harness) in [
        ("other", "%7", Harness::Pi),
        ("session", "%8", Harness::Pi),
        ("session", "%7", Harness::Codex),
        ("pending-42", "%7", Harness::Pi),
    ] {
        let e = armed();
        observe(&e, session, pane, harness, REFERENCE);
        let facts = e.snapshot(id()).unwrap();
        assert!(facts.receipt.is_none());
        assert!(!facts.probably_landed);
    }
    let e = armed();
    e.observe(
        &SessionId::new("session"),
        None,
        Harness::Pi,
        &LifecycleEvent::PromptSubmit {
            prompt: Some(REFERENCE.into()),
        },
    )
    .unwrap();
    assert!(e.receipt(id()).unwrap().is_none());
}

#[test]
fn duplicate_exact_hooks_are_idempotent() {
    let e = armed();
    exact(&e);
    let receipt = e.receipt(id()).unwrap().unwrap();
    exact(&e);
    assert_eq!(e.receipt(id()).unwrap(), Some(receipt.clone()));
    let wrong = DeliveryReceipt {
        session_id: SessionId::new("wrong"),
        ..receipt.clone()
    };
    assert!(e.acknowledge(id(), &wrong).is_err());
    assert_eq!(e.receipt(id()).unwrap(), Some(receipt.clone()));
    e.acknowledge(id(), &receipt).unwrap();
    e.acknowledge(id(), &receipt).unwrap();
    assert!(matches!(e.receipt(id()), Err(MessageError::Unavailable(_))));
}

#[test]
fn lazy_first_prompt_accepts_new_eligible_session_outside_baseline() {
    let e = evidence();
    e.register(id(), REFERENCE.into()).unwrap();
    let mut s = scope();
    s.session_id = None;
    s.excluded_sessions.insert(SessionId::new("old"));
    e.arm(s).unwrap();
    for session in ["old", "pending-42"] {
        observe(&e, session, "%7", Harness::Pi, REFERENCE);
        assert!(e.receipt(id()).unwrap().is_none());
    }
    observe(&e, "new", "%7", Harness::Pi, REFERENCE);
    assert_eq!(e.receipt(id()).unwrap().unwrap().session_id.as_str(), "new");
}

#[test]
fn new_pane_occupant_is_not_old_attempt_evidence() {
    let e = armed();
    observe(&e, "replacement", "%7", Harness::Pi, REFERENCE);
    assert!(!e.snapshot(id()).unwrap().probably_landed);
    let lazy = evidence();
    lazy.register(id(), REFERENCE.into()).unwrap();
    lazy.arm(AttemptScope {
        session_id: None,
        ..scope()
    })
    .unwrap();
    lazy.observe(
        &SessionId::new("first"),
        Some("%7"),
        Harness::Pi,
        &LifecycleEvent::WorkingStart,
    )
    .unwrap();
    observe(&lazy, "replacement", "%7", Harness::Pi, REFERENCE);
    assert!(lazy.receipt(id()).unwrap().is_none());
    observe(&lazy, "first", "%7", Harness::Pi, REFERENCE);
    assert_eq!(
        lazy.receipt(id()).unwrap().unwrap().session_id.as_str(),
        "first"
    );
}

#[tokio::test(start_paused = true)]
async fn activity_before_arm_does_not_latch_and_after_arm_uses_receive_order() {
    let e = evidence();
    e.register(id(), REFERENCE.into()).unwrap();
    e.observe_activity(&SessionId::new("session"), Some("%7"))
        .unwrap();
    e.arm(scope()).unwrap();
    assert!(!e.snapshot(id()).unwrap().probably_landed);
    // No clock advance: receive order, not a strictly increasing UTC test.
    e.observe_activity(&SessionId::new("session"), Some("%7"))
        .unwrap();
    assert!(e.snapshot(id()).unwrap().probably_landed);
    // Repeated arm of the same applied attempt must not erase evidence.
    e.arm(scope()).unwrap();
    assert!(e.snapshot(id()).unwrap().probably_landed);
    assert!(e.receipt(id()).unwrap().is_none());
}

#[test]
fn older_attempt_receipt_survives_new_arm_and_failure() {
    let e = armed();
    e.observe_activity(&SessionId::new("session"), Some("%7"))
        .unwrap();
    e.arm(AttemptScope {
        attempt_seq: 130,
        session_id: Some(SessionId::new("new")),
        ..scope()
    })
    .unwrap();
    assert!(!e.snapshot(id()).unwrap().probably_landed);
    // No durable failure callback erases scopes; Failed is not receipt-terminal.
    exact(&e);
    assert_eq!(
        e.receipt(id()).unwrap().unwrap().session_id.as_str(),
        "session"
    );
    e.arm(AttemptScope {
        attempt_seq: 140,
        ..scope()
    })
    .unwrap();
    assert!(e.receipt(id()).unwrap().is_some());
}

#[tokio::test(start_paused = true)]
async fn coalesced_wake_retains_receipt_and_receive_time() {
    let e = armed();
    let received = e.clock().now().unwrap();
    exact(&e);
    exact(&e);
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    e.wake.notified().await;
    assert!(e.receipt(id()).unwrap().is_some());
    let state = e.state().unwrap();
    let activity = state.sessions.get(&SessionId::new("session")).unwrap();
    assert_eq!(activity.0, received);
    assert_eq!(activity.1.as_ref().unwrap().as_str(), "%7");
}

#[test]
fn capacity_refuses_without_evicting_or_disabling() {
    for resource in ["messages", "scopes", "baseline"] {
        let e = armed();
        exact(&e);
        let refused = match resource {
            "messages" => (1..=MAX_MESSAGES)
                .map(|n| {
                    let id: MessageId = format!("m{}", n + 1000).parse().unwrap();
                    e.register(id, crate::org::message_delivery::inbox_reference(id))
                })
                .find_map(Result::err),
            "scopes" => (1..=MAX_SCOPES)
                .map(|n| {
                    e.arm(AttemptScope {
                        attempt_seq: 124 + n as u64,
                        ..scope()
                    })
                })
                .find_map(Result::err),
            _ => e
                .arm(AttemptScope {
                    attempt_seq: 125,
                    excluded_sessions: (0..=MAX_SESSIONS)
                        .map(|n| SessionId::new(format!("old-{n}")))
                        .collect(),
                    ..scope()
                })
                .err(),
        };
        assert!(
            matches!(refused, Some(MessageError::Unavailable(_))),
            "{resource}"
        );
        // The refusal is the caller's alone: nothing is evicted, and the
        // retained confirmation stays readable.
        assert!(e.receipt(id()).unwrap().is_some(), "{resource}");
    }
}

#[test]
fn poisoning_disables_even_previously_retained_receipts() {
    let e = armed();
    exact(&e);
    let shared = e.clone();
    assert!(std::thread::spawn(move || {
        let _lock = shared.state.lock().unwrap();
        panic!("synthetic poison");
    })
    .join()
    .is_err());
    assert!(matches!(e.receipt(id()), Err(MessageError::Unavailable(_))));
    assert!(matches!(
        e.snapshot(id()),
        Err(MessageError::Unavailable(_))
    ));
}

#[test]
fn stale_or_conflicting_arm_cannot_clear_landing() {
    let e = armed();
    exact(&e);
    let before = e.snapshot(id()).unwrap();
    for bad in [
        // Same applied attempt, another pane.
        AttemptScope {
            pane: PaneId::parse("%8").unwrap(),
            ..scope()
        },
        // An older attempt sequence.
        AttemptScope {
            attempt_seq: 100,
            ..scope()
        },
        // Another recipient.
        AttemptScope {
            attempt_seq: 130,
            recipient: "demo/other".parse().unwrap(),
            ..scope()
        },
    ] {
        assert!(matches!(e.arm(bad), Err(MessageError::Refused(_))));
        assert_eq!(e.snapshot(id()).unwrap(), before);
    }
    assert!(before.probably_landed && before.receipt.is_some());
}

#[test]
fn bad_arm_refuses_only_its_caller() {
    let e = armed();
    exact(&e);
    let before = e.snapshot(id()).unwrap();
    let other: MessageId = "m200".parse().unwrap();
    e.register(other, "[m200] Read: atm inbox --id m200".into())
        .unwrap();
    for bad in [
        // No applied DeliveryAttempted has sequence 0.
        AttemptScope {
            message_id: other,
            attempt_seq: 0,
            ..scope()
        },
        // A pending ID can never confirm.
        AttemptScope {
            message_id: other,
            session_id: Some(SessionId::new("pending-42")),
            ..scope()
        },
        // The bound session is in its own excluded baseline.
        AttemptScope {
            message_id: other,
            excluded_sessions: HashSet::from([SessionId::new("session")]),
            ..scope()
        },
    ] {
        assert!(matches!(e.arm(bad), Err(MessageError::Refused(_))));
    }
    // m123 keeps its evidence, and m200 can still be armed properly.
    assert_eq!(e.snapshot(id()).unwrap(), before);
    let facts = e.snapshot(other).unwrap();
    assert!(facts.receipt.is_none() && !facts.probably_landed);
    e.arm(AttemptScope {
        message_id: other,
        ..scope()
    })
    .unwrap();
}

#[test]
fn bad_register_refuses_only_its_caller() {
    let e = armed();
    exact(&e);
    let other: MessageId = "m200".parse().unwrap();
    for text in [
        "[m123] Read: atm inbox --id m123",
        "[m200]",
        "[m200] Read: atm inbox --id m200\n",
        "",
    ] {
        assert!(
            matches!(
                e.register(other, text.into()),
                Err(MessageError::Refused(_))
            ),
            "{text:?}"
        );
    }
    // Nothing was stored for m200, and m123 is untouched.
    assert!(matches!(
        e.arm(AttemptScope {
            message_id: other,
            ..scope()
        }),
        Err(MessageError::Unavailable(_))
    ));
    assert!(e.receipt(id()).unwrap().is_some());
    e.register(other, "[m200] Read: atm inbox --id m200".into())
        .unwrap();
}

#[test]
fn receipt_of_unknown_id_is_unavailable() {
    // Ok(None) would say "no receipt" about an attempt evidence knows nothing of.
    let e = armed();
    assert!(e.receipt(id()).unwrap().is_none());
    let unknown: MessageId = "m999".parse().unwrap();
    assert!(matches!(
        e.receipt(unknown),
        Err(MessageError::Unavailable(_))
    ));
    assert!(matches!(
        e.snapshot(unknown),
        Err(MessageError::Unavailable(_))
    ));
}

#[test]
fn acknowledgement_reclaims_scope_and_registration_capacity() {
    let e = armed();
    exact(&e);
    e.acknowledge(id(), &e.receipt(id()).unwrap().unwrap())
        .unwrap();
    assert_eq!(e.state().unwrap().scopes, 0);
    assert!(e.state().unwrap().messages.is_empty());
    assert!(matches!(
        e.snapshot(id()),
        Err(MessageError::Unavailable(_))
    ));
}

#[test]
fn evidence_unavailable_blocks_retry() {
    let e = MessageEvidence::disabled(evidence().clock().clone(), "retention unavailable");
    assert!(matches!(
        e.snapshot(id()),
        Err(MessageError::Unavailable(_))
    ));
    assert!(matches!(e.receipt(id()), Err(MessageError::Unavailable(_))));
}
