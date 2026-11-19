#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers may panic
mod support;
use atm_core::{Money, SessionId};
use atm_org::*;
use chrono::Duration;
use serde_json::json;
use support::*;

fn apply(state: &mut OrgState, seq: u64, event: OrgEvent) {
    state
        .apply(seq, time() + Duration::seconds(seq as i64), &event)
        .unwrap();
}
fn registered() -> OrgState {
    let mut state = OrgState::default();
    apply(
        &mut state,
        1,
        OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/old".into(),
        },
    );
    state
}
fn started() -> OrgState {
    let mut state = registered();
    apply(
        &mut state,
        2,
        OrgEvent::AgentUpRequested {
            agent_id: id(),
            role: role(),
        },
    );
    state
}

#[test]
fn project_registration_moves_stable_root() {
    let mut state = registered();
    apply(
        &mut state,
        2,
        OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/new".into(),
        },
    );
    assert_eq!(state.project(&"p".parse().unwrap()).unwrap().root, "/new");
    assert_eq!(state.projects().count(), 1);
}

#[test]
fn every_agent_event_applies_its_effect() {
    let mut state = started();
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Starting);
    assert_eq!(state.agent(&id()).unwrap().role.as_ref(), Some(&role()));
    apply(
        &mut state,
        3,
        OrgEvent::AgentPaneCreated {
            agent_id: id(),
            pane: "%1".into(),
        },
    );
    assert_eq!(state.agent_for_pane("%1").unwrap().id, id());
    apply(
        &mut state,
        4,
        OrgEvent::AgentSessionBound {
            agent_id: id(),
            session_id: "s1".into(),
        },
    );
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Ready);
    apply(
        &mut state,
        5,
        OrgEvent::AgentClearRequested { agent_id: id() },
    );
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Clearing);
    assert_eq!(state.agent(&id()).unwrap().session_id, None);
    apply(
        &mut state,
        6,
        OrgEvent::AgentSessionBound {
            agent_id: id(),
            session_id: "s2".into(),
        },
    );
    apply(&mut state, 7, OrgEvent::AgentSessionLost { agent_id: id() });
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Starting);
    assert_eq!(state.agent(&id()).unwrap().session_id, None);
    assert_eq!(state.agent(&id()).unwrap().pane.as_deref(), Some("%1"));
    apply(
        &mut state,
        8,
        OrgEvent::AgentStuck {
            agent_id: id(),
            reason: "late".into(),
        },
    );
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Stuck);
    assert_eq!(state.agent(&id()).unwrap().reason.as_deref(), Some("late"));
    apply(
        &mut state,
        9,
        OrgEvent::AgentDown {
            agent_id: id(),
            reason: "done".into(),
        },
    );
    let agent = state.agent(&id()).unwrap();
    assert_eq!(agent.status, AgentStatus::Down);
    assert_eq!(agent.pane, None);
    assert_eq!(agent.session_id, None);
    assert_eq!(agent.updated_at, time() + Duration::seconds(9));
    assert!(state.agent_for_pane("%1").is_none());
    let other = "p/other".parse().unwrap();
    apply(
        &mut state,
        10,
        OrgEvent::AgentAdopted {
            agent_id: other,
            pane: "%1".into(),
            session_id: None,
        },
    );
    assert_eq!(
        state.agent_for_pane("%1").unwrap().status,
        AgentStatus::Starting
    );
    assert_eq!(state.agent_for_pane("%1").unwrap().role, None);
    apply(
        &mut state,
        11,
        OrgEvent::AgentAdopted {
            agent_id: id(),
            pane: "%2".into(),
            session_id: Some("s3".into()),
        },
    );
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Ready);
    assert_eq!(state.agent(&id()).unwrap().role.as_ref(), Some(&role()));
    assert_eq!(state.agent(&id()).unwrap().reason, None);
}

#[test]
fn editor_ready_is_effect_only_and_preserves_binding_facts() {
    for bound in [false, true] {
        let mut state = started();
        apply(
            &mut state,
            3,
            OrgEvent::AgentPaneCreated {
                agent_id: id(),
                pane: "%1".into(),
            },
        );
        apply(
            &mut state,
            4,
            OrgEvent::AgentSessionBound {
                agent_id: id(),
                session_id: "old".into(),
            },
        );
        if !bound {
            apply(
                &mut state,
                5,
                OrgEvent::AgentClearRequested { agent_id: id() },
            );
        }
        apply(
            &mut state,
            6,
            OrgEvent::AgentStuck {
                agent_id: id(),
                reason: "dialog".into(),
            },
        );
        let before = state.agent(&id()).unwrap().clone();
        apply(&mut state, 7, OrgEvent::AgentEditorReady { agent_id: id() });
        let agent = state.agent(&id()).unwrap();
        assert_eq!(agent.status, AgentStatus::Ready);
        assert_eq!(agent.reason, None);
        assert_eq!(agent.updated_at, time() + Duration::seconds(7));
        assert_eq!(agent.session_id, before.session_id);
        assert_eq!(agent.excluded_session_id, before.excluded_session_id);
        assert_eq!(agent.pane, before.pane);
        // A committed obsolete binding is a fact, not a command to revalidate.
        apply(
            &mut state,
            8,
            OrgEvent::AgentSessionBound {
                agent_id: id(),
                session_id: "old".into(),
            },
        );
        assert_eq!(state.agent(&id()).unwrap().session_id, Some("old".into()));
        assert_eq!(state.agent(&id()).unwrap().excluded_session_id, None);
    }
    let mut state = started();
    apply(
        &mut state,
        3,
        OrgEvent::AgentDown {
            agent_id: id(),
            reason: "gone".into(),
        },
    );
    apply(&mut state, 4, OrgEvent::AgentEditorReady { agent_id: id() });
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Ready);
    assert_eq!(state.agent(&id()).unwrap().pane, None);
    let before = state.clone();
    assert!(state
        .apply(
            5,
            time(),
            &OrgEvent::AgentEditorReady {
                agent_id: "p/missing".parse().unwrap()
            }
        )
        .is_err());
    assert_eq!(state, before);
}

#[test]
fn message_effects_keep_delivery_timestamps_and_sequence_identity() {
    let mut state = started();
    let mid = MessageId::new(40);
    apply(
        &mut state,
        40,
        OrgEvent::MessageSent(content(Address::Agent(id()))),
    );
    let msg = state.message(&mid).unwrap();
    assert_eq!(msg.id, mid);
    assert_eq!(msg.status, DeliveryStatus::Queued);
    assert_eq!(msg.sent_at, time() + Duration::seconds(40));
    apply(
        &mut state,
        41,
        OrgEvent::MessageDeliveryAttempted {
            message_id: mid,
            attempt: 1,
            pane: "%1".into(),
        },
    );
    assert_eq!(
        state.message(&mid).unwrap().status,
        DeliveryStatus::Delivering
    );
    assert_eq!(
        state.message(&mid).unwrap().last_attempt_at,
        Some(time() + Duration::seconds(41))
    );
    apply(
        &mut state,
        42,
        OrgEvent::MessageDeliveryFailed {
            message_id: mid,
            reason: "retry".into(),
            final_failure: false,
        },
    );
    assert_eq!(state.message(&mid).unwrap().status, DeliveryStatus::Queued);
    assert_eq!(
        state.message(&mid).unwrap().failed_at,
        Some(time() + Duration::seconds(42))
    );
    apply(
        &mut state,
        43,
        OrgEvent::MessageDeliveryAttempted {
            message_id: mid,
            attempt: 2,
            pane: "%1".into(),
        },
    );
    apply(
        &mut state,
        44,
        OrgEvent::MessageDelivered {
            message_id: mid,
            session_id: "s1".into(),
            confirmation: "token".into(),
        },
    );
    let msg = state.message(&mid).unwrap();
    assert_eq!(msg.status, DeliveryStatus::Delivered);
    assert_eq!(msg.delivered_at, Some(time() + Duration::seconds(44)));
    assert_eq!(msg.receipt.as_ref().unwrap().confirmation, "token");
    apply(
        &mut state,
        45,
        OrgEvent::MessageRead {
            message_ids: vec![mid],
        },
    );
    apply(
        &mut state,
        46,
        OrgEvent::MessageRead {
            message_ids: vec![mid],
        },
    );
    assert_eq!(
        state.message(&mid).unwrap().read_at,
        Some(time() + Duration::seconds(45))
    );
    apply(
        &mut state,
        47,
        OrgEvent::MessageSent(content(Address::Owner)),
    );
    let owner = state.message(&MessageId::new(47)).unwrap();
    assert_eq!(owner.status, DeliveryStatus::Delivered);
    assert_eq!(owner.delivered_at, Some(owner.sent_at));
    apply(
        &mut state,
        48,
        OrgEvent::MessageSent(content(Address::Agent(id()))),
    );
    apply(
        &mut state,
        49,
        OrgEvent::MessageDeliveryFailed {
            message_id: MessageId::new(48),
            reason: "final".into(),
            final_failure: true,
        },
    );
    assert_eq!(
        state.message(&MessageId::new(48)).unwrap().status,
        DeliveryStatus::Failed
    );
    assert_eq!(
        state
            .inbox(&Address::Agent(id()))
            .iter()
            .map(|m| m.id.seq())
            .collect::<Vec<_>>(),
        [40, 48]
    );
    assert_eq!(state.inbox(&Address::Owner).len(), 1);
}

#[test]
fn apply_is_effect_only_not_current_policy() {
    let mut state = started();
    // A durable fact remains applicable even if current command policy forbids it.
    for event in [
        OrgEvent::AgentDown {
            agent_id: id(),
            reason: "done".into(),
        },
        OrgEvent::AgentSessionBound {
            agent_id: id(),
            session_id: "late".into(),
        },
        OrgEvent::AgentSessionBound {
            agent_id: id(),
            session_id: "new".into(),
        },
        OrgEvent::AgentClearRequested { agent_id: id() },
        OrgEvent::AgentClearRequested { agent_id: id() },
    ] {
        apply(&mut state, 3, event);
    }
    apply(
        &mut state,
        4,
        OrgEvent::MessageSent(content(Address::Agent(id()))),
    );
    let mid = MessageId::new(4);
    apply(
        &mut state,
        5,
        OrgEvent::MessageRead {
            message_ids: vec![mid],
        },
    );
    apply(
        &mut state,
        6,
        OrgEvent::MessageDeliveryAttempted {
            message_id: mid,
            attempt: 3,
            pane: "%1".into(),
        },
    );
    apply(
        &mut state,
        7,
        OrgEvent::MessageDeliveryAttempted {
            message_id: mid,
            attempt: 0,
            pane: "%1".into(),
        },
    );
    assert_eq!(
        state
            .message(&mid)
            .unwrap()
            .last_attempt
            .as_ref()
            .unwrap()
            .attempt,
        0
    );
}

#[test]
fn invalid_references_pane_collisions_and_duplicate_messages_are_atomic() {
    let mut state = started();
    apply(
        &mut state,
        3,
        OrgEvent::AgentPaneCreated {
            agent_id: id(),
            pane: "%1".into(),
        },
    );
    apply(
        &mut state,
        4,
        OrgEvent::MessageSent(content(Address::Owner)),
    );
    let before = state.clone();
    for event in [
        OrgEvent::AgentUpRequested {
            agent_id: "missing/lead".parse().unwrap(),
            role: role(),
        },
        OrgEvent::AgentDown {
            agent_id: "p/missing".parse().unwrap(),
            reason: "done".into(),
        },
        OrgEvent::AgentDown {
            agent_id: AgentId {
                project: "p".parse().unwrap(),
                name: "BAD".into(),
            },
            reason: "done".into(),
        },
        OrgEvent::AgentAdopted {
            agent_id: "p/other".parse().unwrap(),
            pane: "%1".into(),
            session_id: None,
        },
        OrgEvent::MessageSent(content(Address::Owner)),
        OrgEvent::MessageRead {
            message_ids: vec![MessageId::new(4), MessageId::new(999)],
        },
    ] {
        assert!(state.apply(4, time(), &event).is_err(), "{event:?}");
        assert_eq!(state, before);
    }
}

#[test]
fn summaries_replace_snapshots_and_queries_are_deterministic() {
    let mut state = started();
    assert_eq!(state.agent_cost(&id()), None);
    let mut first = summary();
    first.session_id = SessionId::new("z");
    first.cost = None;
    apply(&mut state, 3, OrgEvent::SessionSummary(first.clone()));
    assert_eq!(state.agent_cost(&id()), None);
    let mut second = summary();
    second.session_id = SessionId::new("a");
    second.cost = Some(Money::from_microdollars(7));
    apply(&mut state, 4, OrgEvent::SessionSummary(second));
    first.cost = Some(Money::from_microdollars(10));
    apply(&mut state, 5, OrgEvent::SessionSummary(first.clone()));
    first.cost = Some(Money::from_microdollars(20));
    apply(&mut state, 6, OrgEvent::SessionSummary(first));
    assert_eq!(state.agent_cost(&id()), Some(Money::from_microdollars(27)));
    assert_eq!(
        state
            .agent_sessions(&id())
            .iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "z"]
    );
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Starting);
    assert_eq!(
        state.session_summary(&"z".into()).unwrap().cost,
        Some(Money::from_microdollars(20))
    );
    apply(
        &mut state,
        7,
        OrgEvent::AgentAdopted {
            agent_id: "p/alpha".parse().unwrap(),
            pane: "%9".into(),
            session_id: None,
        },
    );
    assert_eq!(
        state
            .agents()
            .map(|a| a.id.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "lead"]
    );
}

#[test]
fn notice_validation_is_atomic() {
    let mut state = started();
    apply(
        &mut state,
        3,
        OrgEvent::MessageSent(content(Address::Agent(id()))),
    );
    let valid = MessageContent {
        from: Address::System,
        kind: MessageKind::Note,
        delivery_notice: Some(DeliveryNotice {
            source_message_id: MessageId::new(3),
            reason: "focus".into(),
        }),
        ..content(Address::Owner)
    };
    let mut invalid = vec![
        MessageContent {
            delivery_notice: Some(DeliveryNotice {
                source_message_id: MessageId::new(999),
                reason: "focus".into(),
            }),
            ..valid.clone()
        },
        MessageContent {
            delivery_notice: Some(DeliveryNotice {
                source_message_id: MessageId::new(3),
                reason: String::new(),
            }),
            ..valid.clone()
        },
        MessageContent {
            from: Address::Owner,
            ..valid.clone()
        },
        MessageContent {
            from: Address::Agent(id()),
            ..valid.clone()
        },
        MessageContent {
            to: Address::Agent(id()),
            ..valid.clone()
        },
        MessageContent {
            to: Address::System,
            ..valid.clone()
        },
    ];
    for kind in [
        MessageKind::Brief,
        MessageKind::Report,
        MessageKind::Question,
        MessageKind::Answer,
        MessageKind::Review,
    ] {
        invalid.push(MessageContent {
            kind,
            ..valid.clone()
        });
    }
    let outcomes: Vec<_> = invalid
        .into_iter()
        .map(|note| {
            let mut candidate = state.clone();
            let refused = candidate
                .apply(4, time(), &OrgEvent::MessageSent(note.clone()))
                .is_err();
            (note, refused, candidate == state)
        })
        .collect();
    assert!(
        outcomes
            .iter()
            .all(|(_, refused, unchanged)| *refused && *unchanged),
        "{outcomes:?}"
    );
    apply(&mut state, 4, OrgEvent::MessageSent(valid.clone()));
    let note = state.message(&MessageId::new(4)).unwrap();
    assert_eq!(note.content, valid);
    assert_eq!(note.status, DeliveryStatus::Delivered);
    assert_eq!(note.read_at, None);
    assert_eq!(note.delivered_at, Some(note.sent_at));
}

// Break caught: a named delivery notice can escape its reason, source or
// audience restrictions, or a rejected event advances state/replay counters.
#[test]
fn named_notice_validation_is_narrow() {
    let mut state = started();
    let worker_id: AgentId = "p/worker".parse().unwrap();
    let worker = Address::Agent(worker_id.clone());
    apply(
        &mut state,
        3,
        OrgEvent::AgentAdopted {
            agent_id: worker_id,
            pane: "%1".into(),
            session_id: None,
        },
    );
    apply(
        &mut state,
        4,
        OrgEvent::MessageSent(content(worker.clone())),
    );
    apply(
        &mut state,
        5,
        OrgEvent::MessageSent(content(Address::Owner)),
    );
    let valid = MessageContent {
        from: Address::System,
        kind: MessageKind::Note,
        delivery_notice: Some(DeliveryNotice {
            source_message_id: MessageId::new(4),
            reason: "blocked.readiness_timeout".into(),
        }),
        ..content(Address::Agent(id()))
    };
    let mut bad = vec![
        (
            "owner sender",
            MessageContent {
                from: Address::Owner,
                ..valid.clone()
            },
        ),
        (
            "agent sender",
            MessageContent {
                from: worker.clone(),
                ..valid.clone()
            },
        ),
        (
            "not a note",
            MessageContent {
                kind: MessageKind::Report,
                ..valid.clone()
            },
        ),
        (
            "unregistered target",
            MessageContent {
                to: Address::Agent("p/absent".parse().unwrap()),
                ..valid.clone()
            },
        ),
        (
            "source seat",
            MessageContent {
                to: worker,
                ..valid.clone()
            },
        ),
        (
            "system target",
            MessageContent {
                to: Address::System,
                ..valid.clone()
            },
        ),
    ];
    for (name, source) in [("owner source", 5), ("absent source", 999)] {
        bad.push((
            name,
            MessageContent {
                delivery_notice: Some(DeliveryNotice {
                    source_message_id: MessageId::new(source),
                    reason: "unobserved".into(),
                }),
                ..valid.clone()
            },
        ));
    }
    for reason in [
        "",
        "focus",
        "running_turn",
        "failed.pane_gone",
        "future_reason",
        "blocked.",
        "hold.",
        "unobserved.extra",
        "evidence_unavailable.extra",
    ] {
        bad.push((
            reason,
            MessageContent {
                delivery_notice: Some(DeliveryNotice {
                    source_message_id: MessageId::new(4),
                    reason: reason.into(),
                }),
                ..valid.clone()
            },
        ));
    }
    let before = state.clone();
    let mut stats = ReplayStats::default();
    for (name, note) in bad {
        let event = OrgEvent::MessageSent(note);
        assert!(
            stats
                .apply_parts(&mut state, 6, time(), event.kind(), &event.to_body())
                .is_err(),
            "accepted {name:?}"
        );
        assert_eq!(state, before, "mutated on {name:?}");
        assert_eq!(stats, ReplayStats::default(), "counted {name:?}");
    }
}

// Break caught: routing a notice about another keyed notice can recursively
// notify managers. The legacy Owner destination must remain usable.
#[test]
fn named_notice_about_another_delivery_notice_refuses_but_owner_fallback_works() {
    let mut state = started();
    let worker: AgentId = "p/worker".parse().unwrap();
    let other: AgentId = "p/other".parse().unwrap();
    for (seq, agent_id, pane) in [(3, worker.clone(), "%1"), (4, other.clone(), "%2")] {
        apply(
            &mut state,
            seq,
            OrgEvent::AgentAdopted {
                agent_id,
                pane: pane.into(),
                session_id: None,
            },
        );
    }
    apply(
        &mut state,
        5,
        OrgEvent::MessageSent(content(Address::Agent(worker))),
    );
    let note = MessageContent {
        from: Address::System,
        kind: MessageKind::Note,
        delivery_notice: Some(DeliveryNotice {
            source_message_id: MessageId::new(5),
            reason: "unobserved".into(),
        }),
        ..content(Address::Agent(id()))
    };
    apply(&mut state, 6, OrgEvent::MessageSent(note.clone()));
    let nested = MessageContent {
        to: Address::Agent(other),
        delivery_notice: Some(DeliveryNotice {
            source_message_id: MessageId::new(6),
            reason: "blocked.agent_unavailable".into(),
        }),
        ..note
    };
    let before = state.clone();
    let mut stats = ReplayStats::default();
    let event = OrgEvent::MessageSent(nested.clone());
    assert!(matches!(
        stats.apply_parts(&mut state, 7, time(), event.kind(), &event.to_body()),
        Err(OrgError::Apply(ApplyError::InvalidDeliveryNotice(_)))
    ));
    assert_eq!(state, before);
    assert_eq!(stats, ReplayStats::default());
    let fallback = MessageContent {
        to: Address::Owner,
        ..nested
    };
    apply(&mut state, 7, OrgEvent::MessageSent(fallback));
    assert_eq!(
        state.inbox(&Address::Owner)[0].status,
        DeliveryStatus::Delivered
    );
}

#[test]
fn duplicate_committed_notice_key_is_atomic() {
    let mut state = OrgState::default();
    apply(
        &mut state,
        1,
        OrgEvent::MessageSent(content(Address::Owner)),
    );
    let valid = MessageContent {
        from: Address::System,
        kind: MessageKind::Note,
        delivery_notice: Some(DeliveryNotice {
            source_message_id: MessageId::new(1),
            reason: "focus".into(),
        }),
        ..content(Address::Owner)
    };
    apply(&mut state, 2, OrgEvent::MessageSent(valid.clone()));
    let before = state.clone();
    let changed = MessageContent {
        body: "Different diagnostic".into(),
        ..valid
    };
    assert!(state
        .apply(3, time(), &OrgEvent::MessageSent(changed))
        .is_err());
    assert_eq!(state, before);
    assert!(state.message(&MessageId::new(3)).is_none());
}

#[test]
fn each_control_kind_is_a_complete_state_noop() {
    let mut state = started();
    let before = state.clone();
    let audit = ControlAudit {
        pane: "%unmanaged".into(),
        action: json!({"opaque":true}),
    };
    for event in [
        OrgEvent::ControlSent(audit.clone()),
        OrgEvent::ControlSpawned(audit.clone()),
        OrgEvent::ControlKilled(audit),
    ] {
        apply(&mut state, 100, event);
        assert_eq!(state, before);
    }
}
