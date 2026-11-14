#![allow(clippy::unwrap_used)] // Test fixtures only.
mod support;

use atm_org::*;
use serde_json::json;
use support::{id, role, time};

fn command() -> OrgCommand {
    OrgCommand::Revalidate { agent_id: id() }
}
fn event() -> OrgEvent {
    OrgEvent::AgentRevalidationRequested { agent_id: id() }
}
fn starting() -> OrgState {
    let mut state = OrgState::default();
    for (seq, event) in [
        OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
        },
        OrgEvent::AgentUpRequested {
            agent_id: id(),
            role: role(),
        },
        OrgEvent::AgentPaneCreated {
            agent_id: id(),
            pane: "%1".into(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        state.apply(seq as u64 + 1, time(), &event).unwrap();
    }
    state
}
fn ready_bound() -> OrgState {
    let mut state = starting();
    state
        .apply(
            4,
            time(),
            &OrgEvent::AgentSessionBound {
                agent_id: id(),
                session_id: "recorded".into(),
            },
        )
        .unwrap();
    state
}
fn ready_unbound_with_exclusion() -> OrgState {
    let mut state = ready_bound();
    state
        .apply(5, time(), &OrgEvent::AgentClearRequested { agent_id: id() })
        .unwrap();
    state
        .apply(6, time(), &OrgEvent::AgentEditorReady { agent_id: id() })
        .unwrap();
    state
}

#[test]
fn revalidation_codec_roundtrip_is_additive() {
    let body = json!({"agent_id":"p/lead"});
    assert_eq!(event().kind(), "agent.revalidation_requested");
    assert_eq!(event().to_body(), body);
    assert_eq!(
        OrgEvent::from_parts("agent.revalidation_requested", &body).unwrap(),
        Some(event())
    );
    assert_eq!(
        serde_json::to_value(event()).unwrap(),
        json!({"kind":"agent.revalidation_requested","body":body})
    );
    assert_eq!(
        OrgEvent::from_parts(
            "agent.revalidation_requested",
            &json!({"agent_id":"p/lead","future":true})
        )
        .unwrap(),
        Some(event())
    );
    assert!(OrgEvent::from_parts("agent.revalidation_requested", &json!({})).is_err());
    assert!(
        OrgEvent::from_parts("agent.revalidation_requested", &json!({"agent_id":"bad"})).is_err()
    );
}

#[test]
fn revalidation_ready_with_session_preserves_binding_and_role() {
    let mut state = ready_bound();
    let before = state.agent(&id()).unwrap().clone();
    assert_eq!(state.decide(&command(), time()).unwrap(), vec![event()]);
    let later = time() + chrono::Duration::seconds(1);
    state.apply(5, later, &event()).unwrap();
    let mut expected = before;
    expected.status = AgentStatus::Starting;
    expected.updated_at = later;
    assert_eq!(state.agent(&id()).unwrap(), &expected);
    assert_eq!(expected.session_id.unwrap().as_str(), "recorded");
}

#[test]
fn revalidation_ready_without_session_preserves_exclusion_and_pane() {
    let mut state = ready_unbound_with_exclusion();
    let before = state.agent(&id()).unwrap().clone();
    assert_eq!(state.decide(&command(), time()).unwrap(), vec![event()]);
    state.apply(7, time(), &event()).unwrap();
    let mut expected = before;
    expected.status = AgentStatus::Starting;
    assert_eq!(state.agent(&id()).unwrap(), &expected);
    assert_eq!(expected.session_id, None);
    assert_eq!(expected.pane.as_deref(), Some("%1"));
    assert_eq!(expected.excluded_session_id.unwrap().as_str(), "recorded");
}

#[test]
fn revalidation_starting_with_session_is_idempotent_and_fold_preserves_it() {
    let mut state = ready_bound();
    state.apply(5, time(), &event()).unwrap();
    let before = state.clone();
    assert!(state.decide(&command(), time()).unwrap().is_empty());
    assert_eq!(state, before);
    // Applying a committed duplicate is still a fact, not a policy decision.
    state.apply(6, time(), &event()).unwrap();
    assert_eq!(state, before);
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Starting);
    assert_eq!(
        state
            .agent(&id())
            .unwrap()
            .session_id
            .as_ref()
            .unwrap()
            .as_str(),
        "recorded"
    );
}

#[test]
fn revalidation_refuses_other_source_states_without_mutation() {
    let mut cases = vec![starting()];
    for fact in [
        OrgEvent::AgentClearRequested { agent_id: id() },
        OrgEvent::AgentStuck {
            agent_id: id(),
            reason: "blocked".into(),
        },
        OrgEvent::AgentDown {
            agent_id: id(),
            reason: "gone".into(),
        },
    ] {
        let mut state = ready_bound();
        state.apply(5, time(), &fact).unwrap();
        cases.push(state);
    }
    for state in cases {
        let before = state.clone();
        assert!(matches!(
            state.decide(&command(), time()),
            Err(Refusal::InvalidAgentState { .. })
        ));
        assert_eq!(state, before);
    }
    let state = OrgState::default();
    assert!(matches!(
        state.decide(&command(), time()),
        Err(Refusal::Invariant(ApplyError::UnknownAgent(_)))
    ));
    let invalid = OrgCommand::Revalidate {
        agent_id: AgentId {
            project: "p".parse().unwrap(),
            name: "../bad".into(),
        },
    };
    assert!(matches!(
        ready_bound().decide(&invalid, time()),
        Err(Refusal::Invariant(ApplyError::InvalidId(_)))
    ));
}

#[test]
fn revalidation_replay_preserves_legacy_prefix_and_stored_identity() {
    let prefix = [
        ("project.registered", json!({"project_id":"p","root":"/p"})),
        (
            "agent.adopted",
            json!({"agent_id":"p/lead","pane":"%1","session_id":"recorded"}),
        ),
    ];
    let mut state = OrgState::default();
    let mut stats = ReplayStats::default();
    for (index, (kind, body)) in prefix.iter().enumerate() {
        stats
            .apply_parts(&mut state, index as u64 + 1, time(), kind, body)
            .unwrap();
    }
    let legacy = state.clone();
    assert_eq!(legacy.agent(&id()).unwrap().status, AgentStatus::Ready);
    assert_eq!(stats.applied, 2);
    stats
        .apply_parts(
            &mut state,
            3,
            time(),
            "agent.revalidation_requested",
            &json!({"agent_id":"p/lead"}),
        )
        .unwrap();
    assert_eq!(stats.applied, 3);
    assert_eq!(stats.skipped_unknown, 0);
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Starting);
    assert_eq!(
        state.agent(&id()).unwrap().session_id,
        legacy.agent(&id()).unwrap().session_id
    );
    assert_eq!(
        state.agent(&id()).unwrap().pane,
        legacy.agent(&id()).unwrap().pane
    );
    stats
        .apply_parts(
            &mut state,
            4,
            time(),
            "agent.editor_ready",
            &json!({"agent_id":"p/lead"}),
        )
        .unwrap();
    assert_eq!(state, legacy); // Same time, same binding, same pane; readiness restored.
}

#[test]
fn revalidation_fact_validates_reference_but_not_current_command_policy() {
    let mut missing = OrgState::default();
    let before = missing.clone();
    assert!(matches!(
        missing.apply(1, time(), &event()),
        Err(ApplyError::UnknownAgent(_))
    ));
    assert_eq!(missing, before);
    let mut state = ready_bound();
    state
        .apply(
            5,
            time(),
            &OrgEvent::AgentStuck {
                agent_id: id(),
                reason: "historical".into(),
            },
        )
        .unwrap();
    assert!(state.decide(&command(), time()).is_err());
    state.apply(6, time(), &event()).unwrap();
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Starting);
    assert_eq!(state.agent(&id()).unwrap().reason, None);
    assert_eq!(
        state
            .agent(&id())
            .unwrap()
            .session_id
            .as_ref()
            .unwrap()
            .as_str(),
        "recorded"
    );
}
