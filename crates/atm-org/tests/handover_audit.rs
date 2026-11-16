#![allow(clippy::unwrap_used, clippy::expect_used)] // synthetic test histories
//! The handover's two audit facts: they need a known agent and message,
//! always emit, and change no state.
mod support;
use atm_org::*;
use support::*;

fn with_message() -> (OrgState, MessageId) {
    let mut state = OrgState::default();
    let events = [
        OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
        },
        OrgEvent::AgentUpRequested {
            agent_id: id(),
            role: role(),
        },
        OrgEvent::MessageSent(MessageContent {
            from: Address::System,
            to: Address::Agent(id()),
            kind: MessageKind::Note,
            body: "save".into(),
            attachments: Vec::new(),
            task: None,
        }),
    ];
    for (seq, event) in events.iter().enumerate() {
        state.apply(seq as u64 + 1, time(), event).unwrap();
    }
    (state, MessageId::new(3))
}

#[test]
fn handover_audits_always_emit_and_change_no_state() {
    let (mut state, message_id) = with_message();
    let before = state.clone();
    for (command, kind) in [
        (
            OrgCommand::SaveRequested {
                agent_id: id(),
                message_id,
            },
            "agent.save_requested",
        ),
        (
            OrgCommand::Saved {
                agent_id: id(),
                message_id,
            },
            "agent.saved",
        ),
    ] {
        // Twice: an audit is a fact each time, never a no-op.
        for seq in [4, 5] {
            let events = state.decide(&command, time()).unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].kind(), kind);
            state.apply(seq, time(), &events[0]).unwrap();
            assert_eq!(state, before, "{kind} must not change state");
        }
    }
}

#[test]
fn handover_audits_refuse_an_unknown_agent_or_message() {
    let (mut state, message_id) = with_message();
    let unknown_agent: AgentId = "p/nobody".parse().unwrap();
    let unknown_message = MessageId::new(99);
    for (agent_id, message_id) in [(unknown_agent, message_id), (id(), unknown_message)] {
        for command in [
            OrgCommand::SaveRequested {
                agent_id: agent_id.clone(),
                message_id,
            },
            OrgCommand::Saved {
                agent_id: agent_id.clone(),
                message_id,
            },
        ] {
            assert!(matches!(
                state.decide(&command, time()),
                Err(Refusal::Invariant(
                    ApplyError::UnknownAgent(_) | ApplyError::UnknownMessage(_)
                ))
            ));
        }
        let before = state.clone();
        for event in [
            OrgEvent::AgentSaveRequested {
                agent_id: agent_id.clone(),
                message_id,
            },
            OrgEvent::AgentSaved {
                agent_id: agent_id.clone(),
                message_id,
            },
        ] {
            assert!(state.apply(4, time(), &event).is_err());
            assert_eq!(state, before);
        }
    }
}
