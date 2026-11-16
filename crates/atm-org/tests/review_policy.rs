#![allow(clippy::unwrap_used, clippy::expect_used)] // synthetic test histories
mod support;
use atm_org::*;
use chrono::Duration;
use support::*;

fn ready() -> OrgState {
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
        OrgEvent::AgentSessionBound {
            agent_id: id(),
            session_id: "s1".into(),
        },
    ]
    .iter()
    .enumerate()
    {
        state.apply(seq as u64 + 1, time(), event).unwrap();
    }
    state
}

#[test]
fn registration_conflicts_require_an_explicit_verified_move() {
    let mut state = ready();
    let command = |root: &str, move_root| OrgCommand::RegisterProject {
        project_id: "p".parse().unwrap(),
        root: root.into(),
        move_root,
    };
    assert!(state
        .decide(&command("/p", false), time())
        .unwrap()
        .is_empty());
    assert!(matches!(
        state.decide(&command("/different", false), time()),
        Err(Refusal::ProjectRootConflict { .. })
    ));
    let events = state.decide(&command("/moved", true), time()).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].to_body(),
        serde_json::json!({"project_id":"p","root":"/moved"})
    );
    state.apply(10, time(), &events[0]).unwrap();
    assert_eq!(state.project(&"p".parse().unwrap()).unwrap().root, "/moved");
}

#[test]
fn free_project_ids_resolve_case_unicode_and_truncation_collisions() {
    for folder in ["API", "日本語", &"a".repeat(40)] {
        let mut state = OrgState::default();
        let base = new_project_file(folder).id;
        assert_eq!(state.free_project_id(folder), base);
        let register = |state: &mut OrgState, id: ProjectId| {
            state
                .apply(
                    1,
                    time(),
                    &OrgEvent::ProjectRegistered {
                        project_id: id,
                        root: "/p".into(),
                    },
                )
                .unwrap();
        };
        register(&mut state, base.clone());
        let second = state.free_project_id(folder);
        let expected = format!("{}-2", base.as_str().chars().take(30).collect::<String>());
        assert_eq!(second.to_string(), expected);
        assert!(second.as_str().len() <= 32);
        let third: ProjectId = format!("{}-3", base.as_str().chars().take(30).collect::<String>())
            .parse()
            .unwrap();
        register(&mut state, third.clone());
        assert_eq!(state.free_project_id(folder), second); // fill first gap, not max + 1
        register(&mut state, second);
        assert_eq!(
            state.free_project_id(folder).to_string(),
            format!("{}-4", base.as_str().chars().take(30).collect::<String>())
        );
    }
    let mut state = OrgState::default();
    for seq in 1..=15 {
        let free = state.free_project_id(&"a".repeat(40));
        assert!(free.as_str().len() <= 32);
        assert!(free.to_string().parse::<ProjectId>().is_ok());
        state
            .apply(
                seq,
                time(),
                &OrgEvent::ProjectRegistered {
                    project_id: free,
                    root: "/p".into(),
                },
            )
            .unwrap();
    }
    assert_eq!(
        state.free_project_id(&"a".repeat(40)).to_string(),
        format!("{}-16", "a".repeat(29))
    );
}

#[test]
fn late_confirmations_deliver_from_queued_delivering_and_failed() {
    for failure in [None, Some(false), Some(true)] {
        let mut state = ready();
        let mid = MessageId::new(10);
        state
            .apply(
                10,
                time(),
                &OrgEvent::MessageSent(content(Address::Agent(id()))),
            )
            .unwrap();
        state
            .apply(
                11,
                time(),
                &OrgEvent::MessageDeliveryAttempted {
                    message_id: mid,
                    attempt: 1,
                    pane: "%1".into(),
                },
            )
            .unwrap();
        if let Some(final_failure) = failure {
            state
                .apply(
                    12,
                    time(),
                    &OrgEvent::MessageDeliveryFailed {
                        message_id: mid,
                        reason: "timeout".into(),
                        final_failure,
                    },
                )
                .unwrap();
        }
        let command = OrgCommand::Delivered {
            message_id: mid,
            session_id: "s1".into(),
            confirmation: "late token".into(),
        };
        let ts = time() + Duration::seconds(30);
        let events = state.decide(&command, ts).unwrap();
        assert_eq!(events.len(), 1, "{failure:?}");
        state.apply(13, ts, &events[0]).unwrap();
        let message = state.message(&mid).unwrap();
        assert_eq!(message.status, DeliveryStatus::Delivered);
        assert_eq!(message.delivered_at, Some(ts));
        assert_eq!(message.receipt.as_ref().unwrap().session_id.as_str(), "s1");
        assert_eq!(message.receipt.as_ref().unwrap().confirmation, "late token");
        assert!(state.decide(&command, ts).unwrap().is_empty());
    }
}

#[test]
fn adopt_same_binding_repairs_status() {
    for event in [
        OrgEvent::AgentStuck {
            agent_id: id(),
            reason: "late".into(),
        },
        OrgEvent::AgentClearRequested { agent_id: id() },
    ] {
        let mut state = ready();
        state.apply(5, time(), &event).unwrap();
        let session_id = state.agent(&id()).unwrap().session_id.clone();
        let command = OrgCommand::Adopt {
            agent_id: id(),
            pane: "%1".into(),
            session_id: session_id.clone(),
        };
        let events = state.decide(&command, time()).unwrap();
        assert_eq!(events.len(), 1);
        state.apply(6, time(), &events[0]).unwrap();
        assert_eq!(
            state.agent(&id()).unwrap().status,
            if session_id.is_some() {
                AgentStatus::Ready
            } else {
                AgentStatus::Starting
            }
        );
        assert!(state.decide(&command, time()).unwrap().is_empty());
    }
}

#[test]
fn row_missing_preserves_stuck_without_a_session() {
    let mut state = ready();
    state
        .apply(5, time(), &OrgEvent::AgentSessionLost { agent_id: id() })
        .unwrap();
    state
        .apply(
            6,
            time(),
            &OrgEvent::AgentStuck {
                agent_id: id(),
                reason: "startup timeout".into(),
            },
        )
        .unwrap();
    assert!(state
        .decide(&OrgCommand::SessionLost { agent_id: id() }, time())
        .unwrap()
        .is_empty());
    assert_eq!(
        state.agent(&id()).unwrap().reason.as_deref(),
        Some("startup timeout")
    );
}

#[test]
fn stale_pane_reports_are_noops_in_every_status_but_conflicts_refuse() {
    for event in [
        OrgEvent::AgentSessionBound {
            agent_id: id(),
            session_id: "s1".into(),
        },
        OrgEvent::AgentSessionLost { agent_id: id() },
        OrgEvent::AgentClearRequested { agent_id: id() },
        OrgEvent::AgentStuck {
            agent_id: id(),
            reason: "late".into(),
        },
        OrgEvent::AgentDown {
            agent_id: id(),
            reason: "stop".into(),
        },
    ] {
        let mut state = ready();
        state.apply(5, time(), &event).unwrap();
        // Effect-only replay can record a pane even on a down agent.
        state
            .apply(
                6,
                time(),
                &OrgEvent::AgentPaneCreated {
                    agent_id: id(),
                    pane: "%1".into(),
                },
            )
            .unwrap();
        assert!(state
            .decide(
                &OrgCommand::PaneCreated {
                    agent_id: id(),
                    pane: "%1".into()
                },
                time()
            )
            .unwrap()
            .is_empty());
        assert!(state
            .decide(
                &OrgCommand::PaneCreated {
                    agent_id: id(),
                    pane: "%2".into()
                },
                time()
            )
            .is_err());
        assert!(state
            .decide(
                &OrgCommand::PaneCreated {
                    agent_id: "p/missing".parse().unwrap(),
                    pane: "%1".into()
                },
                time()
            )
            .is_err());
    }
}

#[test]
fn stale_attempts_on_terminal_messages_are_noops() {
    for delivered in [false, true] {
        let mut state = ready();
        let mid = MessageId::new(10);
        state
            .apply(
                10,
                time(),
                &OrgEvent::MessageSent(content(Address::Agent(id()))),
            )
            .unwrap();
        let event = if delivered {
            OrgEvent::MessageDelivered {
                message_id: mid,
                session_id: "s1".into(),
                confirmation: "token".into(),
            }
        } else {
            OrgEvent::MessageDeliveryFailed {
                message_id: mid,
                reason: "failed".into(),
                final_failure: true,
            }
        };
        state.apply(11, time(), &event).unwrap();
        let before = state.clone();
        assert!(state
            .decide(
                &OrgCommand::DeliveryAttempted {
                    message_id: mid,
                    attempt: 1,
                    pane: "%1".into()
                },
                time()
            )
            .unwrap()
            .is_empty());
        assert_eq!(state, before);
    }
}

#[test]
fn clock_step_keeps_summary_and_cost_empty_audit_still_refuses() {
    let mut state = ready();
    let mut s = summary();
    s.ended = Some(s.started - Duration::hours(1));
    let events = state
        .decide(&OrgCommand::RecordSummary(s.clone()), time())
        .unwrap();
    assert_eq!(events.len(), 1);
    state.apply(10, time(), &events[0]).unwrap();
    assert_eq!(state.session_summary(&s.session_id), Some(&s));
    assert_eq!(state.agent_cost(&id()), s.cost);
    let audit = ControlAudit {
        pane: "".into(),
        action: serde_json::json!(null),
    };
    for command in [
        OrgCommand::ControlSent(audit.clone()),
        OrgCommand::ControlSpawned(audit.clone()),
        OrgCommand::ControlKilled(audit),
    ] {
        assert!(state.decide(&command, time()).is_err());
    }
}
