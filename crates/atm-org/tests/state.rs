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
