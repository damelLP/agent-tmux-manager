#![allow(clippy::unwrap_used)] // Test fixtures only.
//! `session.process` and the binding time a restarted daemon matches a
//! discovery row against (s3-d1).
mod support;

use atm_org::*;
use chrono::Duration;
use serde_json::json;
use support::{id, role, time};

fn process(session: &str, pid: u32, start: u64) -> SessionProcess {
    SessionProcess {
        session_id: session.into(),
        pid,
        process_start: start,
    }
}

fn apply_all(state: &mut OrgState, events: &[(i64, OrgEvent)]) {
    for (seq, (offset, event)) in events.iter().enumerate() {
        let ts = time() + Duration::seconds(*offset);
        state.apply(seq as u64 + 1, ts, event).unwrap();
    }
}

fn started() -> Vec<(i64, OrgEvent)> {
    vec![
        (
            0,
            OrgEvent::ProjectRegistered {
                project_id: "p".parse().unwrap(),
                root: "/p".into(),
            },
        ),
        (
            0,
            OrgEvent::AgentUpRequested {
                agent_id: id(),
                role: role(),
            },
        ),
        (
            1,
            OrgEvent::AgentPaneCreated {
                agent_id: id(),
                pane: "%1".into(),
            },
        ),
    ]
}

#[test]
fn session_process_records_latest_and_a_repeat_writes_nothing() {
    let mut state = OrgState::default();
    let first = process("s1", 41, 900);
    let command = OrgCommand::RecordProcess(first.clone());
    assert_eq!(
        state.decide(&command, time()).unwrap(),
        vec![OrgEvent::SessionProcess(first.clone())]
    );
    state
        .apply(1, time(), &OrgEvent::SessionProcess(first.clone()))
        .unwrap();
    assert_eq!(state.session_process(&"s1".into()), Some(&first));
    // The same process again: nothing to write.
    assert_eq!(state.decide(&command, time()).unwrap(), vec![]);
    // The session resumed in another process: the new one is recorded.
    let resumed = process("s1", 77, 1200);
    assert_eq!(
        state
            .decide(&OrgCommand::RecordProcess(resumed.clone()), time())
            .unwrap(),
        vec![OrgEvent::SessionProcess(resumed.clone())]
    );
    state
        .apply(2, time(), &OrgEvent::SessionProcess(resumed.clone()))
        .unwrap();
    assert_eq!(state.session_process(&"s1".into()), Some(&resumed));
    assert_eq!(state.session_process(&"s2".into()), None);
    // A record names no agent, so it needs none.
    assert_eq!(state.agents().count(), 0);
}

#[test]
fn session_process_body_round_trips_and_refuses_bad_facts() {
    let event = OrgEvent::SessionProcess(process("s1", 41, 900));
    let body = json!({"session_id":"s1","pid":41,"process_start":900});
    assert_eq!(event.kind(), "session.process");
    assert_eq!(event.to_body(), body);
    assert_eq!(
        OrgEvent::from_parts("session.process", &body).unwrap(),
        Some(event.clone())
    );
    // Later fields are tolerated, like every known body.
    assert_eq!(
        OrgEvent::from_parts(
            "session.process",
            &json!({"session_id":"s1","pid":41,"process_start":900,"later":true})
        )
        .unwrap(),
        Some(event)
    );
    assert!(OrgEvent::from_parts("session.process", &json!({"session_id":"s1"})).is_err());
    let mut state = OrgState::default();
    for bad in [process("", 41, 900), process("s1", 0, 900)] {
        assert!(state
            .decide(&OrgCommand::RecordProcess(bad.clone()), time())
            .is_err());
        assert!(state
            .apply(1, time(), &OrgEvent::SessionProcess(bad))
            .is_err());
    }
    assert_eq!(state, OrgState::default());
}

#[test]
fn bound_at_follows_the_binding_event() {
    let at = |offset: i64| Some(time() + Duration::seconds(offset));
    let mut state = OrgState::default();
    apply_all(&mut state, &started());
    assert_eq!(state.agent(&id()).unwrap().bound_at, None);

    let mut events = started();
    events.push((
        10,
        OrgEvent::AgentSessionBound {
            agent_id: id(),
            session_id: "s1".into(),
        },
    ));
    // Readiness and revalidation keep the binding and its time.
    events.push((20, OrgEvent::AgentRevalidationRequested { agent_id: id() }));
    events.push((30, OrgEvent::AgentEditorReady { agent_id: id() }));
    let mut state = OrgState::default();
    apply_all(&mut state, &events);
    let agent = state.agent(&id()).unwrap();
    assert_eq!(
        (
            agent.session_id.as_ref().map(|s| s.as_str()),
            agent.bound_at
        ),
        (Some("s1"), at(10))
    );
    assert_eq!(agent.updated_at, at(30).unwrap());

    // Every event that ends the binding ends its time.
    for (ending, rebinds) in [
        (OrgEvent::AgentClearRequested { agent_id: id() }, true),
        (OrgEvent::AgentSessionLost { agent_id: id() }, true),
        (
            OrgEvent::AgentDown {
                agent_id: id(),
                reason: "gone".into(),
            },
            false,
        ),
    ] {
        let mut state = OrgState::default();
        let mut events = events.clone();
        events.push((40, ending));
        apply_all(&mut state, &events);
        assert_eq!(state.agent(&id()).unwrap().bound_at, None);
        if rebinds {
            let next = events.len() as u64 + 1;
            state
                .apply(
                    next,
                    time() + Duration::seconds(50),
                    &OrgEvent::AgentSessionBound {
                        agent_id: id(),
                        session_id: "s2".into(),
                    },
                )
                .unwrap();
            assert_eq!(state.agent(&id()).unwrap().bound_at, at(50));
        }
    }

    // Adopt with a session binds at its time; adopt without one has none.
    for (session, expected) in [(Some("s3"), at(60)), (None, None)] {
        let mut state = OrgState::default();
        let mut events = events.clone();
        events.push((
            40,
            OrgEvent::AgentDown {
                agent_id: id(),
                reason: "gone".into(),
            },
        ));
        events.push((
            60,
            OrgEvent::AgentAdopted {
                agent_id: id(),
                pane: "%2".into(),
                session_id: session.map(Into::into),
            },
        ));
        apply_all(&mut state, &events);
        assert_eq!(state.agent(&id()).unwrap().bound_at, expected);
    }
}
