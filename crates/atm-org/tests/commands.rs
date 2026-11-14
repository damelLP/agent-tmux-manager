mod support;
use atm_org::*;
use support::*;

fn run(state: &mut OrgState, command: OrgCommand, seq: &mut u64) -> Vec<OrgEvent> {
    let before = state.clone();
    let events = state.decide(&command, time()).unwrap();
    assert_eq!(*state, before);
    for event in &events {
        *seq += 1;
        state.apply(*seq, time(), event).unwrap();
    }
    events
}
fn setup() -> (OrgState, u64) {
    let mut state = OrgState::default();
    let mut seq = 0;
    run(
        &mut state,
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
        },
        &mut seq,
    );
    (state, seq)
}
fn up() -> OrgCommand {
    OrgCommand::Up {
        agent_id: id(),
        role: role(),
    }
}
fn pane() -> OrgCommand {
    OrgCommand::PaneCreated {
        agent_id: id(),
        pane: "%1".into(),
    }
}
fn bound(session: &str) -> OrgCommand {
    OrgCommand::SessionBound {
        agent_id: id(),
        session_id: session.into(),
    }
}

#[test]
fn agent_matrix_and_repeated_facts() {
    let (mut state, mut seq) = setup();
    assert_eq!(run(&mut state, up(), &mut seq).len(), 1);
    assert!(state.decide(&up(), time()).is_err());
    assert!(state
        .decide(&OrgCommand::Clear { agent_id: id() }, time())
        .is_err());
    assert!(state.decide(&bound("s1"), time()).is_err());
    assert_eq!(run(&mut state, pane(), &mut seq).len(), 1);
    assert!(run(&mut state, pane(), &mut seq).is_empty());
    assert_eq!(run(&mut state, bound("s1"), &mut seq).len(), 1);
    assert!(run(&mut state, bound("s1"), &mut seq).is_empty());
    assert!(state.decide(&up(), time()).is_err());
    // Manual clear/new session can bind while already ready.
    assert_eq!(run(&mut state, bound("s2"), &mut seq).len(), 1);
    assert_eq!(
        run(&mut state, OrgCommand::Clear { agent_id: id() }, &mut seq).len(),
        1
    );
    assert!(state.decide(&up(), time()).is_err());
    assert!(run(
        &mut state,
        OrgCommand::SessionLost { agent_id: id() },
        &mut seq
    )
    .is_empty());
    run(&mut state, bound("s3"), &mut seq);
    assert_eq!(
        run(
            &mut state,
            OrgCommand::SessionLost { agent_id: id() },
            &mut seq
        )
        .len(),
        1
    );
    assert!(run(
        &mut state,
        OrgCommand::SessionLost { agent_id: id() },
        &mut seq
    )
    .is_empty());
    let stuck = OrgCommand::Stuck {
        agent_id: id(),
        reason: "late".into(),
    };
    assert_eq!(run(&mut state, stuck.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, stuck, &mut seq).is_empty());
    assert!(state.decide(&up(), time()).is_err());
    assert_eq!(run(&mut state, bound("late-session"), &mut seq).len(), 1);
    let down = OrgCommand::Down {
        agent_id: id(),
        reason: "stop".into(),
    };
    assert_eq!(run(&mut state, down.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, down, &mut seq).is_empty());
    assert!(state.decide(&bound("no"), time()).is_err());
    assert!(state
        .decide(
            &OrgCommand::Stuck {
                agent_id: id(),
                reason: "no".into()
            },
            time()
        )
        .is_err());
    assert!(run(
        &mut state,
        OrgCommand::SessionLost { agent_id: id() },
        &mut seq
    )
    .is_empty());
    assert_eq!(run(&mut state, up(), &mut seq).len(), 1);
}

#[test]
fn adopt_pane_ownership_and_respawn() {
    let (mut state, mut seq) = setup();
    let adopt = OrgCommand::Adopt {
        agent_id: id(),
        pane: "%1".into(),
        session_id: Some("s1".into()),
    };
    assert_eq!(run(&mut state, adopt.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, adopt, &mut seq).is_empty());
    for command in [
        OrgCommand::Adopt {
            agent_id: "p/other".parse().unwrap(),
            pane: "%1".into(),
            session_id: None,
        },
        OrgCommand::Adopt {
            agent_id: id(),
            pane: "%2".into(),
            session_id: None,
        },
        pane(),
    ] {
        assert!(state.decide(&command, time()).is_err());
    }
    let events = run(
        &mut state,
        OrgCommand::Respawn {
            agent_id: id(),
            role: role(),
            reason: "again".into(),
        },
        &mut seq,
    );
    assert_eq!(
        events.iter().map(OrgEvent::kind).collect::<Vec<_>>(),
        ["agent.down", "agent.up_requested"]
    );
    assert_eq!(state.agent(&id()).unwrap().status, AgentStatus::Starting);
}

#[test]
fn project_move_unknown_references_invalid_ids_and_role_mismatch() {
    let (mut state, mut seq) = setup();
    assert!(run(
        &mut state,
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/p".into()
        },
        &mut seq
    )
    .is_empty());
    assert_eq!(
        run(
            &mut state,
            OrgCommand::RegisterProject {
                project_id: "p".parse().unwrap(),
                root: "/renamed".into()
            },
            &mut seq
        )
        .len(),
        1
    );
    for command in [
        OrgCommand::Up {
            agent_id: "unknown/lead".parse().unwrap(),
            role: role(),
        },
        OrgCommand::Up {
            agent_id: "p/different".parse().unwrap(),
            role: role(),
        },
        OrgCommand::Clear { agent_id: id() },
        OrgCommand::SendMessage(content(Address::Agent(id()))),
        OrgCommand::SendMessage(content(Address::System)),
        OrgCommand::Up {
            agent_id: AgentId {
                project: "p".parse().unwrap(),
                name: "BAD".into(),
            },
            role: role(),
        },
    ] {
        let before = state.clone();
        assert!(state.decide(&command, time()).is_err());
        assert_eq!(state, before);
    }
}

#[test]
fn delivery_policy_and_read_ownership() {
    let (mut state, mut seq) = setup();
    run(&mut state, up(), &mut seq);
    run(
        &mut state,
        OrgCommand::SendMessage(content(Address::Agent(id()))),
        &mut seq,
    );
    let mid = MessageId::new(seq);
    let delivered = OrgCommand::Delivered {
        message_id: mid,
        session_id: "s1".into(),
        confirmation: "token".into(),
    };
    assert!(state.decide(&delivered, time()).is_err());
    let read = OrgCommand::ReadMessages {
        who: Address::Agent(id()),
        message_ids: vec![mid],
    };
    assert!(state.decide(&read, time()).is_err());
    let attempt = OrgCommand::DeliveryAttempted {
        message_id: mid,
        attempt: 1,
        pane: "%1".into(),
    };
    assert_eq!(run(&mut state, attempt.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, attempt.clone(), &mut seq).is_empty());
    let failure = OrgCommand::DeliveryFailed {
        message_id: mid,
        reason: "retry".into(),
        final_failure: false,
    };
    assert_eq!(run(&mut state, failure.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, failure, &mut seq).is_empty());
    assert!(state.decide(&attempt, time()).is_err()); // previous attempt cannot be reused
    run(
        &mut state,
        OrgCommand::DeliveryAttempted {
            message_id: mid,
            attempt: 2,
            pane: "%1".into(),
        },
        &mut seq,
    );
    assert_eq!(run(&mut state, delivered.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, delivered, &mut seq).is_empty());
    assert!(state
        .decide(
            &OrgCommand::ReadMessages {
                who: Address::Owner,
                message_ids: vec![mid]
            },
            time()
        )
        .is_err());
    assert_eq!(run(&mut state, read.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, read, &mut seq).is_empty());
}

#[test]
fn summary_deduplicates_but_control_audits_always_emit() {
    let (mut state, mut seq) = setup();
    run(&mut state, up(), &mut seq);
    let command = OrgCommand::RecordSummary(summary());
    assert_eq!(run(&mut state, command.clone(), &mut seq).len(), 1);
    assert!(run(&mut state, command, &mut seq).is_empty());
    let audit = ControlAudit {
        pane: "%unmanaged".into(),
        action: serde_json::json!("audit"),
    };
    for command in [
        OrgCommand::ControlSent(audit.clone()),
        OrgCommand::ControlSpawned(audit.clone()),
        OrgCommand::ControlKilled(audit),
    ] {
        assert_eq!(run(&mut state, command.clone(), &mut seq).len(), 1);
        assert_eq!(run(&mut state, command, &mut seq).len(), 1);
    }
}
